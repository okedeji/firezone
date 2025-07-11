use crate::{
    auth, deep_link,
    gui::{self, system_tray},
    ipc::{self, SocketId},
    logging, service,
    settings::{self, AdvancedSettings},
    updates, uptime,
};
use anyhow::{Context, Result, anyhow, bail};
use connlib_model::ResourceView;
use firezone_bin_shared::DnsControlMethod;
use firezone_logging::FilterReloadHandle;
use firezone_telemetry::Telemetry;
use futures::{
    SinkExt, Stream, StreamExt,
    stream::{self, BoxStream},
};
use secrecy::{ExposeSecret as _, SecretString};
use std::{
    collections::BTreeSet,
    ops::ControlFlow,
    path::PathBuf,
    task::Poll,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use url::Url;

mod ran_before;

pub type CtlrTx = mpsc::Sender<ControllerRequest>;

pub struct Controller<I: GuiIntegration> {
    /// Debugging-only settings like API URL, auth URL, log filter
    advanced_settings: AdvancedSettings,
    // Sign-in state with the portal / deep links
    auth: auth::Auth,
    clear_logs_callback: Option<oneshot::Sender<Result<(), String>>>,
    ctlr_tx: CtlrTx,
    ipc_client: ipc::ClientWrite<service::ClientMsg>,
    ipc_rx: ipc::ClientRead<service::ServerMsg>,
    integration: I,
    log_filter_reloader: FilterReloadHandle,
    /// A release that's ready to download
    release: Option<updates::Release>,
    rx: ReceiverStream<ControllerRequest>,
    status: Status,
    updates_rx: ReceiverStream<Option<updates::Notification>>,
    uptime: uptime::Tracker,

    gui_ipc_clients: BoxStream<
        'static,
        Result<(
            ipc::ServerRead<gui::ClientMsg>,
            ipc::ServerWrite<gui::ServerMsg>,
        )>,
    >,

    dns_notifier: BoxStream<'static, Result<()>>,
    network_notifier: BoxStream<'static, Result<()>>,
}

pub trait GuiIntegration {
    fn set_welcome_window_visible(
        &self,
        visible: bool,
        current_session: Option<&auth::Session>,
    ) -> Result<()>;

    fn notify_signed_in(&self, session: &auth::Session) -> Result<()>;
    fn notify_signed_out(&self) -> Result<()>;

    /// Also opens non-URLs
    fn open_url<P: AsRef<str>>(&self, url: P) -> Result<()>;

    fn set_tray_icon(&mut self, icon: system_tray::Icon);
    fn set_tray_menu(&mut self, app_state: system_tray::AppState);
    fn show_notification(&self, title: &str, body: &str) -> Result<()>;
    fn show_update_notification(&self, ctlr_tx: CtlrTx, title: &str, url: url::Url) -> Result<()>;

    /// Shows a window that the system tray knows about, e.g. not Welcome.
    fn show_window(&self, window: system_tray::Window) -> Result<()>;
}

pub enum ControllerRequest {
    /// The GUI wants us to use these settings in-memory, they've already been saved to disk
    ApplySettings(Box<AdvancedSettings>),
    /// Clear the GUI's logs and await the Tunnel service to clear its logs
    ClearLogs(oneshot::Sender<Result<(), String>>),
    /// The same as the arguments to `client::logging::export_logs_to`
    ExportLogs {
        path: PathBuf,
        stem: PathBuf,
    },
    Fail(Failure),
    GetAdvancedSettings(oneshot::Sender<AdvancedSettings>),
    SignIn,
    SignOut,
    SystemTrayMenu(system_tray::Event),
    UpdateNotificationClicked(Url),
}

// The failure flags are all mutually exclusive
// TODO: I can't figure out from the `clap` docs how to do this:
// `app --fail-on-purpose crash-in-wintun-worker`
// So the failure should be an `Option<Enum>` but _not_ a subcommand.
// You can only have one subcommand per container, I've tried
#[derive(Debug)]
pub enum Failure {
    Crash,
    Error,
    Panic,
}

#[derive(derive_more::Debug)]
pub enum Status {
    /// Firezone is disconnected.
    Disconnected,
    /// At least one connection request has failed, due to failing to reach the Portal, and we are waiting for a network change before we try again
    RetryingConnection {
        /// The token to log in to the Portal, for retrying the connection request.
        #[debug(skip)]
        token: SecretString,
    },
    Quitting, // The user asked to quit and we're waiting for the tunnel daemon to gracefully disconnect so we can flush telemetry.
    /// Firezone is ready to use.
    TunnelReady {
        #[debug(skip)]
        resources: Vec<ResourceView>,
    },
    /// Firezone is signing in to the Portal.
    WaitingForPortal {
        /// The instant when we sent our most recent connect request.
        start_instant: Instant,
        /// The token to log in to the Portal, in case we need to retry the connection request.
        #[debug(skip)]
        token: SecretString,
    },
    /// Firezone has connected to the Portal and is raising the tunnel.
    WaitingForTunnel {
        /// The instant when we sent our most recent connect request.
        start_instant: Instant,
    },
}

impl Default for Status {
    fn default() -> Self {
        Self::Disconnected
    }
}

impl Status {
    /// True if we want to hear about DNS and network changes.
    fn needs_network_changes(&self) -> bool {
        match self {
            Status::Disconnected | Status::RetryingConnection { .. } => false,
            Status::Quitting => false,
            Status::TunnelReady { .. }
            | Status::WaitingForPortal { .. }
            | Status::WaitingForTunnel { .. } => true,
        }
    }

    /// True if we should react to `OnUpdateResources`
    fn needs_resource_updates(&self) -> bool {
        match self {
            Status::Disconnected
            | Status::RetryingConnection { .. }
            | Status::Quitting
            | Status::WaitingForPortal { .. } => false,
            Status::TunnelReady { .. } | Status::WaitingForTunnel { .. } => true,
        }
    }

    fn internet_resource(&self) -> Option<ResourceView> {
        #[expect(clippy::wildcard_enum_match_arm)]
        match self {
            Status::TunnelReady { resources } => {
                resources.iter().find(|r| r.is_internet_resource()).cloned()
            }
            _ => None,
        }
    }
}

enum EventloopTick {
    NetworkChanged(Result<()>),
    DnsChanged(Result<()>),
    IpcMsg(Option<Result<service::ServerMsg>>),
    ControllerRequest(Option<ControllerRequest>),
    UpdateNotification(Option<Option<updates::Notification>>),
    NewInstanceLaunched(
        Option<
            Result<(
                ipc::ServerRead<gui::ClientMsg>,
                ipc::ServerWrite<gui::ServerMsg>,
            )>,
        >,
    ),
}

#[derive(Debug, thiserror::Error)]
#[error("Failed to receive hello: {0:#}")]
pub struct FailedToReceiveHello(anyhow::Error);

impl<I: GuiIntegration> Controller<I> {
    pub(crate) async fn start(
        ctlr_tx: CtlrTx,
        integration: I,
        rx: mpsc::Receiver<ControllerRequest>,
        advanced_settings: AdvancedSettings,
        log_filter_reloader: FilterReloadHandle,
        updates_rx: mpsc::Receiver<Option<updates::Notification>>,
        gui_ipc: ipc::Server,
    ) -> Result<()> {
        tracing::debug!("Starting new instance of `Controller`");

        let (mut ipc_rx, ipc_client) =
            ipc::connect(SocketId::Tunnel, ipc::ConnectOptions::default()).await?;

        receive_hello(&mut ipc_rx)
            .await
            .map_err(FailedToReceiveHello)?;

        let dns_notifier = new_dns_notifier().await?.boxed();
        let network_notifier = new_network_notifier().await?.boxed();

        let controller = Controller {
            advanced_settings,
            auth: auth::Auth::new()?,
            clear_logs_callback: None,
            ctlr_tx,
            ipc_client,
            ipc_rx,
            integration,
            log_filter_reloader,
            release: None,
            rx: ReceiverStream::new(rx),
            status: Default::default(),
            updates_rx: ReceiverStream::new(updates_rx),
            uptime: Default::default(),
            dns_notifier,
            network_notifier,
            gui_ipc_clients: stream::unfold(gui_ipc, |mut gui_ipc| async move {
                let result = gui_ipc.next_client_split().await;

                Some((result, gui_ipc))
            })
            .boxed(),
        };

        controller.main_loop().await?;

        Ok(())
    }

    pub async fn main_loop(mut self) -> Result<()> {
        self.update_telemetry_context().await?;

        if let Some(token) = self
            .auth
            .token()
            .context("Failed to load token from disk during app start")?
        {
            self.start_session(token).await?;
        } else {
            tracing::info!("No token / actor_name on disk, starting in signed-out state");
            self.refresh_system_tray_menu();
        }

        if !ran_before::get().await? {
            self.integration
                .set_welcome_window_visible(true, self.auth.session())?;
        }

        while let Some(tick) = self.tick().await {
            match tick {
                EventloopTick::NetworkChanged(Ok(())) => {
                    if self.status.needs_network_changes() {
                        tracing::debug!("Internet up/down changed, calling `Session::reset`");
                        self.send_ipc(&service::ClientMsg::Reset).await?
                    }

                    self.try_retry_connection().await?
                }
                EventloopTick::DnsChanged(Ok(())) => {
                    if self.status.needs_network_changes() {
                        let resolvers = firezone_bin_shared::system_resolvers_for_gui()?;
                        tracing::debug!(
                            ?resolvers,
                            "New DNS resolvers, calling `Session::set_dns`"
                        );
                        self.send_ipc(&service::ClientMsg::SetDns(resolvers))
                            .await?;
                    }

                    self.try_retry_connection().await?
                }
                EventloopTick::NetworkChanged(Err(e)) | EventloopTick::DnsChanged(Err(e)) => {
                    return Err(e);
                }

                EventloopTick::IpcMsg(msg) => {
                    let msg = msg
                        .context("IPC closed")?
                        .context("Failed to read from IPC")?;

                    match self.handle_service_ipc_msg(msg).await? {
                        ControlFlow::Break(()) => break,
                        ControlFlow::Continue(()) => continue,
                    };
                }

                EventloopTick::ControllerRequest(Some(req)) => self.handle_request(req).await?,
                EventloopTick::ControllerRequest(None) => {
                    tracing::warn!("Controller channel closed, breaking main loop");
                    break;
                }
                EventloopTick::UpdateNotification(Some(notification)) => {
                    self.handle_update_notification(notification)?
                }
                EventloopTick::UpdateNotification(None) => {
                    return Err(anyhow!("Update checker task stopped"));
                }
                EventloopTick::NewInstanceLaunched(None) => {
                    return Err(anyhow!("GUI IPC socket closed"));
                }
                EventloopTick::NewInstanceLaunched(Some(Err(e))) => {
                    tracing::warn!("Failed to accept IPC connection from new GUI instance: {e:#}");
                }
                EventloopTick::NewInstanceLaunched(Some(Ok((mut read, mut write)))) => {
                    let client_msg = read.next().await;

                    if let Err(e) = self.handle_gui_ipc_msg(client_msg).await {
                        tracing::debug!("Failed to handle IPC message from new GUI instance: {e:#}")
                    }

                    if let Err(e) = write.send(&gui::ServerMsg::Ack).await {
                        tracing::debug!("Failed to ack IPC message from new GUI instance: {e:#}")
                    }
                }
            }
        }

        tracing::debug!("Closing...");

        if let Err(error) = self.ipc_client.close().await {
            tracing::error!("ipc_client: {error:#}");
        }

        // Don't close telemetry here, `run` will close it.

        Ok(())
    }

    async fn tick(&mut self) -> Option<EventloopTick> {
        std::future::poll_fn(|cx| {
            if let Poll::Ready(Some(res)) = self.dns_notifier.poll_next_unpin(cx) {
                return Poll::Ready(Some(EventloopTick::DnsChanged(res)));
            }

            if let Poll::Ready(Some(res)) = self.network_notifier.poll_next_unpin(cx) {
                return Poll::Ready(Some(EventloopTick::NetworkChanged(res)));
            }

            if let Poll::Ready(maybe_ipc) = self.ipc_rx.poll_next_unpin(cx) {
                return Poll::Ready(Some(EventloopTick::IpcMsg(maybe_ipc)));
            }

            if let Poll::Ready(maybe_req) = self.rx.poll_next_unpin(cx) {
                return Poll::Ready(Some(EventloopTick::ControllerRequest(maybe_req)));
            }

            if let Poll::Ready(notification) = self.updates_rx.poll_next_unpin(cx) {
                return Poll::Ready(Some(EventloopTick::UpdateNotification(notification)));
            }

            if let Poll::Ready(new_instance) = self.gui_ipc_clients.poll_next_unpin(cx) {
                return Poll::Ready(Some(EventloopTick::NewInstanceLaunched(new_instance)));
            }

            Poll::Pending
        })
        .await
    }

    async fn start_session(&mut self, token: SecretString) -> Result<()> {
        match self.status {
            Status::Disconnected | Status::RetryingConnection { .. } => {}
            Status::Quitting => Err(anyhow!("Can't connect to Firezone, we're quitting"))?,
            Status::TunnelReady { .. } => Err(anyhow!(
                "Can't connect to Firezone, we're already connected."
            ))?,
            Status::WaitingForPortal { .. } | Status::WaitingForTunnel { .. } => Err(anyhow!(
                "Can't connect to Firezone, we're already connecting."
            ))?,
        }

        let api_url = self.advanced_settings.api_url.clone();
        tracing::info!(api_url = api_url.to_string(), "Starting connlib...");

        // Count the start instant from before we connect
        let start_instant = Instant::now();
        self.send_ipc(&service::ClientMsg::Connect {
            api_url: api_url.to_string(),
            token: token.expose_secret().clone(),
        })
        .await?;

        // Change the status after we begin connecting
        self.status = Status::WaitingForPortal {
            start_instant,
            token,
        };

        let session = self.auth.session().context("Missing session")?;
        self.integration.notify_signed_in(session)?;

        self.refresh_system_tray_menu();

        Ok(())
    }

    async fn update_telemetry_context(&mut self) -> Result<()> {
        let environment = self.advanced_settings.api_url.to_string();
        let account_slug = self.auth.session().map(|s| s.account_slug.to_owned());

        if let Some(account_slug) = account_slug.clone() {
            Telemetry::set_account_slug(account_slug);
        }

        self.send_ipc(&service::ClientMsg::StartTelemetry {
            environment,
            release: crate::RELEASE.to_string(),
            account_slug,
        })
        .await?;

        Ok(())
    }

    async fn handle_deep_link(&mut self, url: &Url) -> Result<()> {
        let auth_response =
            deep_link::parse_auth_callback(url).context("Couldn't parse scheme request")?;

        tracing::info!("Received deep link over IPC");

        // Uses `std::fs`
        let token = self
            .auth
            .handle_response(auth_response)
            .context("Couldn't handle auth response")?;

        self.update_telemetry_context().await?;
        self.start_session(token).await?;

        Ok(())
    }

    async fn handle_request(&mut self, req: ControllerRequest) -> Result<()> {
        use ControllerRequest::*;

        match req {
            ApplySettings(settings) => {
                self.log_filter_reloader
                    .reload(&settings.log_filter)
                    .context("Couldn't reload log filter")?;

                self.advanced_settings = *settings;

                self.send_ipc(&service::ClientMsg::ApplyLogFilter {
                    directives: self.advanced_settings.log_filter.clone(),
                })
                .await?;

                tracing::debug!("Applied new settings. Log level will take effect immediately.");

                // Refresh the menu in case the favorites were reset.
                self.refresh_system_tray_menu();
            }
            ClearLogs(completion_tx) => {
                if self.clear_logs_callback.is_some() {
                    tracing::error!(
                        "Can't clear logs, we're already waiting on another log-clearing operation"
                    );
                }
                if let Err(error) = logging::clear_gui_logs().await {
                    tracing::error!("Failed to clear GUI logs: {error:#}");
                }
                self.send_ipc(&service::ClientMsg::ClearLogs).await?;
                self.clear_logs_callback = Some(completion_tx);
            }
            ExportLogs { path, stem } => logging::export_logs_to(path, stem)
                .await
                .context("Failed to export logs to zip")?,
            Fail(Failure::Crash) => {
                tracing::error!("Crashing on purpose");
                // SAFETY: Crashing is unsafe
                unsafe { sadness_generator::raise_segfault() }
            }
            Fail(Failure::Error) => Err(anyhow!("Test error"))?,
            Fail(Failure::Panic) => panic!("Test panic"),
            GetAdvancedSettings(tx) => {
                tx.send(self.advanced_settings.clone()).ok();
            }
            SignIn | SystemTrayMenu(system_tray::Event::SignIn) => {
                let req = self
                    .auth
                    .start_sign_in()
                    .context("Couldn't start sign-in flow")?;

                let url = req.to_url(&self.advanced_settings.auth_base_url);
                self.refresh_system_tray_menu();
                self.integration
                    .open_url(url.expose_secret())
                    .context("Couldn't open auth page")?;
                self.integration
                    .set_welcome_window_visible(false, self.auth.session())?;
            }
            SystemTrayMenu(system_tray::Event::AddFavorite(resource_id)) => {
                self.advanced_settings
                    .favorite_resources
                    .insert(resource_id);
                self.refresh_favorite_resources().await?;
            }
            SystemTrayMenu(system_tray::Event::AdminPortal) => self
                .integration
                .open_url(&self.advanced_settings.auth_base_url)
                .context("Couldn't open auth page")?,
            SystemTrayMenu(system_tray::Event::Copy(s)) => arboard::Clipboard::new()
                .context("Couldn't access clipboard")?
                .set_text(s)
                .context("Couldn't copy resource URL or other text to clipboard")?,
            SystemTrayMenu(system_tray::Event::CancelSignIn) => match &self.status {
                Status::Disconnected
                | Status::RetryingConnection { .. }
                | Status::WaitingForPortal { .. } => {
                    tracing::info!("Calling `sign_out` to cancel sign-in");
                    self.sign_out().await?;
                }
                Status::Quitting => tracing::error!("Can't cancel sign-in while already quitting"),
                Status::TunnelReady { .. } => tracing::error!(
                    "Can't cancel sign-in, the tunnel is already up. This is a logic error in the code."
                ),
                Status::WaitingForTunnel { .. } => {
                    tracing::debug!(
                        "Connlib is already raising the tunnel, calling `sign_out` anyway"
                    );
                    self.sign_out().await?;
                }
            },
            SystemTrayMenu(system_tray::Event::RemoveFavorite(resource_id)) => {
                self.advanced_settings
                    .favorite_resources
                    .remove(&resource_id);
                self.refresh_favorite_resources().await?;
            }
            SystemTrayMenu(system_tray::Event::RetryPortalConnection) => {
                self.try_retry_connection().await?
            }
            SystemTrayMenu(system_tray::Event::EnableInternetResource) => {
                self.advanced_settings.internet_resource_enabled = Some(true);
                self.update_disabled_resources().await?;
            }
            SystemTrayMenu(system_tray::Event::DisableInternetResource) => {
                self.advanced_settings.internet_resource_enabled = Some(false);
                self.update_disabled_resources().await?;
            }
            SystemTrayMenu(system_tray::Event::ShowWindow(window)) => {
                self.integration.show_window(window)?;
                // When the About or Settings windows are hidden / shown, log the
                // run ID and uptime. This makes it easy to check client stability on
                // dev or test systems without parsing the whole log file.
                let uptime_info = self.uptime.info();
                tracing::debug!(
                    uptime_s = uptime_info.uptime.as_secs(),
                    run_id = uptime_info.run_id.to_string(),
                    "Uptime info"
                );
            }
            SignOut | SystemTrayMenu(system_tray::Event::SignOut) => {
                tracing::info!("User asked to sign out");
                self.sign_out().await?;
            }
            SystemTrayMenu(system_tray::Event::Url(url)) => self
                .integration
                .open_url(&url)
                .context("Couldn't open URL from system tray")?,
            SystemTrayMenu(system_tray::Event::Quit) => {
                tracing::info!("User clicked Quit in the menu");
                self.status = Status::Quitting;
                self.send_ipc(&service::ClientMsg::Disconnect).await?;
                self.refresh_system_tray_menu();
            }
            UpdateNotificationClicked(download_url) => {
                tracing::info!("UpdateNotificationClicked in run_controller!");
                self.integration
                    .open_url(&download_url)
                    .context("Couldn't open update page")?;
            }
        }
        Ok(())
    }

    async fn handle_service_ipc_msg(&mut self, msg: service::ServerMsg) -> Result<ControlFlow<()>> {
        match msg {
            service::ServerMsg::ClearedLogs(result) => {
                let Some(tx) = self.clear_logs_callback.take() else {
                    return Err(anyhow!(
                        "Can't handle `IpcClearedLogs` when there's no callback waiting for a `ClearLogs` result"
                    ));
                };
                tx.send(result)
                    .map_err(|_| anyhow!("Couldn't send `ClearLogs` result to Tauri task"))?;
            }
            service::ServerMsg::ConnectResult(result) => {
                self.handle_connect_result(result).await?;
            }
            service::ServerMsg::DisconnectedGracefully => {
                if let Status::Quitting = self.status {
                    return Ok(ControlFlow::Break(()));
                }
            }
            service::ServerMsg::OnDisconnect {
                error_msg,
                is_authentication_error,
            } => {
                self.sign_out().await?;
                if is_authentication_error {
                    tracing::info!(?error_msg, "Auth error");
                    self.integration.show_notification(
                        "Firezone disconnected",
                        "To access resources, sign in again.",
                    )?;
                } else {
                    tracing::error!("Connlib disconnected: {error_msg}");
                    native_dialog::MessageDialog::new()
                        .set_title("Firezone Error")
                        .set_text(&error_msg)
                        .set_type(native_dialog::MessageType::Error)
                        .show_alert()
                        .context("Couldn't show Disconnected alert")?;
                }
            }
            service::ServerMsg::OnUpdateResources(resources) => {
                if !self.status.needs_resource_updates() {
                    return Ok(ControlFlow::Continue(()));
                }
                tracing::debug!(len = resources.len(), "Got new Resources");
                self.status = Status::TunnelReady { resources };
                self.refresh_system_tray_menu();

                self.update_disabled_resources().await?;
            }
            service::ServerMsg::TerminatingGracefully => {
                tracing::info!("Tunnel service exited gracefully");
                self.integration
                    .set_tray_icon(system_tray::icon_terminating());
                self.integration.show_notification(
                    "Firezone disconnected",
                    "The Firezone Tunnel service was shutdown, quitting GUI process.",
                )?;

                return Ok(ControlFlow::Break(()));
            }
            service::ServerMsg::TunnelReady => {
                let Status::WaitingForTunnel { start_instant } = self.status else {
                    // If we are not waiting for a tunnel, continue.
                    return Ok(ControlFlow::Continue(()));
                };

                tracing::info!(elapsed = ?start_instant.elapsed(), "Tunnel ready");
                self.status = Status::TunnelReady { resources: vec![] };
                self.integration.show_notification(
                    "Firezone connected",
                    "You are now signed in and able to access resources.",
                )?;
                self.refresh_system_tray_menu();
            }
            service::ServerMsg::Hello => {}
        }
        Ok(ControlFlow::Continue(()))
    }

    async fn handle_gui_ipc_msg(
        &mut self,
        maybe_msg: Option<Result<gui::ClientMsg>>,
    ) -> Result<()> {
        let client_msg = maybe_msg
            .context("No message received")?
            .context("Failed to read message")?;

        match client_msg {
            gui::ClientMsg::Deeplink(url) => match self.handle_deep_link(&url).await {
                Ok(()) => {}
                Err(error)
                    if error
                        .root_cause()
                        .downcast_ref::<auth::Error>()
                        .is_some_and(|e| matches!(e, auth::Error::NoInflightRequest)) =>
                {
                    tracing::debug!("Ignoring deep-link; no local state");
                }
                Err(error) => {
                    tracing::error!("`handle_deep_link` failed: {error:#}");
                }
            },
            gui::ClientMsg::NewInstance => {
                self.integration
                    .set_welcome_window_visible(true, self.auth.session())?;
            }
        }

        Ok(())
    }

    async fn handle_connect_result(
        &mut self,
        result: Result<(), service::ConnectError>,
    ) -> Result<()> {
        let Status::WaitingForPortal {
            start_instant,
            token,
        } = &self.status
        else {
            tracing::debug!(current_state = ?self.status, "Ignoring `ConnectResult`");

            return Ok(());
        };

        match result {
            Ok(()) => {
                ran_before::set().await?;
                self.status = Status::WaitingForTunnel {
                    start_instant: *start_instant,
                };
                self.refresh_system_tray_menu();
                Ok(())
            }
            Err(service::ConnectError::Io(error)) => {
                // This is typically something like, we don't have Internet access so we can't
                // open the PhoenixChannel's WebSocket.
                tracing::info!(
                    error,
                    "Failed to connect to Firezone Portal, will try again when the network changes"
                );
                self.status = Status::RetryingConnection {
                    token: token.expose_secret().clone().into(),
                };
                self.refresh_system_tray_menu();
                Ok(())
            }
            Err(service::ConnectError::Other(error)) => {
                // We log this here directly instead of forwarding it because errors hard-abort the event-loop and we still want to be able to export logs and stuff.
                // See <https://github.com/firezone/firezone/issues/6547>.
                tracing::error!("Failed to connect to Firezone: {error}");
                self.sign_out().await?;

                Ok(())
            }
        }
    }

    /// Set (or clear) update notification
    fn handle_update_notification(
        &mut self,
        notification: Option<updates::Notification>,
    ) -> Result<()> {
        let Some(notification) = notification else {
            self.release = None;
            self.refresh_system_tray_menu();
            return Ok(());
        };

        let release = notification.release;
        self.release = Some(release.clone());
        self.refresh_system_tray_menu();

        if notification.tell_user {
            let title = format!("Firezone {} available for download", release.version);

            // We don't need to route through the controller here either, we could
            // use the `open` crate directly instead of Tauri's wrapper
            // `tauri::api::shell::open`
            self.integration.show_update_notification(
                self.ctlr_tx.clone(),
                &title,
                release.download_url,
            )?;
        }
        Ok(())
    }

    async fn update_disabled_resources(&mut self) -> Result<()> {
        settings::save(&self.advanced_settings).await?;

        let Some(internet_resource) = self.status.internet_resource() else {
            return Ok(());
        };

        let mut disabled_resources = BTreeSet::new();

        if !self.advanced_settings.internet_resource_enabled() {
            disabled_resources.insert(internet_resource.id());
        }

        self.send_ipc(&service::ClientMsg::SetDisabledResources(
            disabled_resources,
        ))
        .await?;
        self.refresh_system_tray_menu();

        Ok(())
    }

    /// Saves the current settings (including favorites) to disk and refreshes the tray menu
    async fn refresh_favorite_resources(&mut self) -> Result<()> {
        settings::save(&self.advanced_settings).await?;
        self.refresh_system_tray_menu();
        Ok(())
    }

    /// Builds a new system tray menu and applies it to the app
    fn refresh_system_tray_menu(&mut self) {
        // TODO: Refactor `Controller` and the auth module so that "Are we logged in?"
        // doesn't require such complicated control flow to answer.
        let connlib = if let Some(auth_session) = self.auth.session() {
            match &self.status {
                Status::Disconnected => {
                    tracing::error!("We have an auth session but no connlib session");
                    system_tray::ConnlibState::SignedOut
                }
                Status::Quitting => system_tray::ConnlibState::Quitting,
                Status::RetryingConnection { .. } => system_tray::ConnlibState::RetryingConnection,
                Status::TunnelReady { resources } => {
                    system_tray::ConnlibState::SignedIn(system_tray::SignedIn {
                        actor_name: auth_session.actor_name.clone(),
                        favorite_resources: self.advanced_settings.favorite_resources.clone(),
                        internet_resource_enabled: self.advanced_settings.internet_resource_enabled,
                        resources: resources.clone(),
                    })
                }
                Status::WaitingForPortal { .. } => system_tray::ConnlibState::WaitingForPortal,
                Status::WaitingForTunnel { .. } => system_tray::ConnlibState::WaitingForTunnel,
            }
        } else if self.auth.ongoing_request().is_some() {
            // Signing in, waiting on deep link callback
            system_tray::ConnlibState::WaitingForBrowser
        } else {
            system_tray::ConnlibState::SignedOut
        };

        self.integration.set_tray_menu(system_tray::AppState {
            connlib,
            release: self.release.clone(),
        });
    }

    /// If we're in the `RetryingConnection` state, use the token to retry the Portal connection
    async fn try_retry_connection(&mut self) -> Result<()> {
        let token = match &self.status {
            Status::Disconnected
            | Status::Quitting
            | Status::TunnelReady { .. }
            | Status::WaitingForPortal { .. }
            | Status::WaitingForTunnel { .. } => return Ok(()),
            Status::RetryingConnection { token } => token,
        };
        tracing::debug!("Retrying Portal connection...");
        self.start_session(token.expose_secret().clone().into())
            .await?;
        Ok(())
    }

    /// Deletes the auth token, stops connlib, and refreshes the tray menu
    async fn sign_out(&mut self) -> Result<()> {
        match self.status {
            Status::Quitting => return Ok(()),
            Status::Disconnected
            | Status::RetryingConnection { .. }
            | Status::TunnelReady { .. }
            | Status::WaitingForPortal { .. }
            | Status::WaitingForTunnel { .. } => {}
        }
        self.auth.sign_out()?;
        self.integration.notify_signed_out()?;
        self.status = Status::Disconnected;
        tracing::debug!("disconnecting connlib");
        // This is redundant if the token is expired, in that case
        // connlib already disconnected itself.
        self.send_ipc(&service::ClientMsg::Disconnect).await?;
        self.refresh_system_tray_menu();
        Ok(())
    }

    async fn send_ipc(&mut self, msg: &service::ClientMsg) -> Result<()> {
        self.ipc_client
            .send(msg)
            .await
            .context("Failed to send IPC message")
    }
}

async fn new_dns_notifier() -> Result<impl Stream<Item = Result<()>>> {
    let worker = firezone_bin_shared::new_dns_notifier(
        tokio::runtime::Handle::current(),
        DnsControlMethod::default(),
    )
    .await?;

    Ok(stream::try_unfold(worker, |mut worker| async move {
        let () = worker.notified().await?;

        Ok(Some(((), worker)))
    }))
}

async fn new_network_notifier() -> Result<impl Stream<Item = Result<()>>> {
    let worker = firezone_bin_shared::new_network_notifier(
        tokio::runtime::Handle::current(),
        DnsControlMethod::default(),
    )
    .await?;

    Ok(stream::try_unfold(worker, |mut worker| async move {
        let () = worker.notified().await?;

        Ok(Some(((), worker)))
    }))
}

async fn receive_hello(ipc_rx: &mut ipc::ClientRead<service::ServerMsg>) -> Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(5);

    let server_msg = tokio::time::timeout(TIMEOUT, ipc_rx.next())
        .await
        .with_context(|| {
            format!("Timeout while waiting for message from tunnel service for {TIMEOUT:?}")
        })?
        .context("No message received from tunnel service")?
        .context("Failed to receive message from tunnel service")?;

    if !matches!(server_msg, service::ServerMsg::Hello) {
        bail!("Expected `Hello` from tunnel service but got `{server_msg}`")
    }

    Ok(())
}
