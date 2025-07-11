//
//  SettingsView.swift
//  (c) 2024 Firezone, Inc.
//  LICENSE: Apache-2.0
//

// TODO: Refactor to fix file length
// swiftlint:disable file_length

import Combine
import OSLog
import SwiftUI

enum SettingsViewError: Error {
  case logFolderIsUnavailable
  case configurationNotInitialized

  var localizedDescription: String {
    switch self {
    case .logFolderIsUnavailable:
      return """
        Log folder is unavailable.
        Try restarting your device or reinstalling Firezone if this issue persists.
      """
    case .configurationNotInitialized:
      return """
        Configuration is not initialized.
        Try restarting your device or reinstalling Firezone if this issue persists.
      """
    }
  }
}

extension FileManager {
  enum FileManagerError: Error {
    case invalidURL(URL, Error)

    var localizedDescription: String {
      switch self {
      case .invalidURL(let url, let error):
        return "Unable to get resource value for '\(url)': \(error)"
      }
    }
  }

  func forEachFileUnder(
    _ dirURL: URL,
    including resourceKeys: Set<URLResourceKey>,
    handler: (URL, URLResourceValues) -> Void
  ) {
    // Deep-traverses the directory at dirURL
    guard
      let enumerator = self.enumerator(
        at: dirURL,
        includingPropertiesForKeys: [URLResourceKey](resourceKeys),
        options: [],
        errorHandler: nil
      )
    else {
      return
    }

    for item in enumerator.enumerated() {
      if Task.isCancelled { break }
      guard let url = item.element as? URL else { continue }
      do {
        let resourceValues = try url.resourceValues(forKeys: resourceKeys)
        handler(url, resourceValues)
      } catch {
        Log.error(FileManagerError.invalidURL(url, error))
      }
    }
  }
}

@MainActor
class SettingsViewModel: ObservableObject {
  private let store: Store
  private var cancellables = Set<AnyCancellable>()

  @Published var settings: Settings

  @Published private(set) var shouldDisableApplyButton = false
  @Published private(set) var shouldDisableResetButton = false

  init(store: Store) {
    self.store = store

    guard let configuration = store.configuration
    else {
      fatalError("Configuration must be initialized to view settings")
    }

    self.settings = Settings(from: configuration)
    setupObservers()

    // TODO: Handle updates to configuration while the SettingsView is open?
  }

  func reset() {
    settings.reset()
    objectWillChange.send()
    updateDerivedState()
  }

  func save() async throws {
    try await store.applySettingsToConfiguration(settings)

    guard let configuration = store.configuration
    else {
      throw SettingsViewError.configurationNotInitialized
    }

    self.settings = Settings(from: configuration)
    setupObservers()
  }

  private func setupObservers() {
    // These all need to be the same underlying type
    Publishers.MergeMany([
      settings.$authURL,
      settings.$apiURL,
      settings.$accountSlug,
      settings.$logFilter
    ])
    .receive(on: RunLoop.main)
    .sink(receiveValue: { [weak self] _ in
      self?.updateDerivedState()
    })
    .store(in: &cancellables)

    Publishers.MergeMany([
      settings.$connectOnStart,
      settings.$startOnLogin
    ])
    .receive(on: RunLoop.main)
    .sink(receiveValue: { [weak self] _ in
      self?.updateDerivedState()
    })
    .store(in: &cancellables)
  }

  private func updateDerivedState() {
    self.shouldDisableApplyButton = (
      settings.areAllFieldsOverridden() ||
      settings.isSaved() ||
      !settings.isValid()
    )

    self.shouldDisableResetButton = (
      settings.areAllFieldsOverridden() ||
      settings.isDefault()
    )
  }

}

// TODO: Move business logic to ViewModel to remove dependency on Store and fix body length
// swiftlint:disable:next type_body_length
public struct SettingsView: View {
  @StateObject private var viewModel: SettingsViewModel
  @Environment(\.dismiss) var dismiss
  @EnvironmentObject var errorHandler: GlobalErrorHandler

  private let store: Store

  private enum ConfirmationAlertContinueAction: Int {
    case none
    case saveSettings
    case saveAllSettingsAndDismiss

    func performAction(on view: SettingsView) async throws {
      switch self {
      case .none:
        break
      case .saveSettings:
        try await view.saveSettings()
      case .saveAllSettingsAndDismiss:
        try await view.saveAllSettingsAndDismiss()
      }
    }
  }

  @State private var isCalculatingLogsSize = false
  @State private var calculatedLogsSize = "Unknown"
  @State private var isClearingLogs = false
  @State private var isExportingLogs = false
  @State private var isShowingConfirmationAlert = false
  @State private var confirmationAlertContinueAction: ConfirmationAlertContinueAction = .none

  @State private var calculateLogSizeTask: Task<(), Never>?

  #if os(iOS)
    @State private var logTempZipFileURL: URL?
    @State private var isPresentingExportLogShareSheet = false
  #endif

  private struct PlaceholderText {
    static let authBaseURL = "Admin portal base URL"
    static let apiURL = "Control plane WebSocket URL"
    static let logFilter = "RUST_LOG-style filter string"
    static let accountSlug = "Account slug or ID (optional)"
  }

  private struct FootnoteText {
    static let forAdvanced = try? AttributedString(
      markdown: """
        **WARNING:** These settings are intended for internal debug purposes **only**. \
        Changing these will disrupt access to your Firezone resources.
        """
    )
  }

  public init(store: Store) {
    self.store = store
    _viewModel = StateObject(wrappedValue: SettingsViewModel(store: store))
  }

  public var body: some View {
#if os(iOS)
    NavigationView {
      ZStack {
        Color(UIColor.systemGroupedBackground)
          .ignoresSafeArea()

        VStack {
          TabView {
            generalTab
              .tabItem {
                Image(systemName: "slider.horizontal.3")
                Text("General")
              }
            advancedTab
              .tabItem {
                Image(systemName: "gearshape.2")
                Text("Advanced")
              }
              .badge(viewModel.settings.isValid() ? nil : "!")
            logsTab
              .tabItem {
                Image(systemName: "doc.text")
                Text("Diagnostic Logs")
              }
          }
        }
        .padding(.bottom, 10)
        .toolbar {
          ToolbarItem(placement: .navigationBarTrailing) {
            Button("Save") {
              let action = ConfirmationAlertContinueAction.saveAllSettingsAndDismiss
              if case .connected = store.vpnStatus {
                self.confirmationAlertContinueAction = action
                self.isShowingConfirmationAlert = true
              } else {
                withErrorHandler { try await action.performAction(on: self) }
              }
            }
            .disabled(viewModel.shouldDisableApplyButton)
          }
          ToolbarItem(placement: .navigationBarLeading) {
            Button("Cancel") { dismiss() }
          }
        }
        .navigationTitle("Settings")
        .navigationBarTitleDisplayMode(.inline)
        .alert(
          "Saving settings will sign you out",
          isPresented: $isShowingConfirmationAlert,
          presenting: confirmationAlertContinueAction,
          actions: { confirmationAlertContinueAction in
            Button("Cancel", role: .cancel) {
              // Nothing to do
            }
            Button("Continue") {
              withErrorHandler { try await confirmationAlertContinueAction.performAction(on: self) }
            }
          },
          message: { _ in
            Text("Changing settings will sign you out and disconnect you from resources")
          }
        )
      }
    }
    #elseif os(macOS)
      VStack {
        TabView {
          generalTab
            .tabItem {
              Text("General")
            }
          advancedTab
            .tabItem {
              Text("Advanced")
            }
          logsTab
            .tabItem {
              Text("Diagnostic Logs")
            }
        }
        .padding(20)
        Spacer()
        HStack(spacing: 5) {
          Text("Build: \(BundleHelper.gitSha)")
            .textSelection(.enabled)
            .foregroundColor(.gray)
          Spacer()
          Button(
            "Reset to Defaults",
            action: {
              viewModel.reset()
            }
          )
          .disabled(viewModel.shouldDisableResetButton)

          Button(
            "Apply",
            action: {
              let action = ConfirmationAlertContinueAction.saveSettings
              if [.connected, .connecting, .reasserting].contains(store.vpnStatus) {
                self.confirmationAlertContinueAction = action
                self.isShowingConfirmationAlert = true
              } else {
                withErrorHandler { try await action.performAction(on: self) }
              }
            }
          )
          .disabled(viewModel.shouldDisableApplyButton)

        }
        .padding([.bottom], 20)
        .padding([.leading, .trailing], 40)
        Spacer()
      }
      .alert(
        "Saving settings will sign you out",
        isPresented: $isShowingConfirmationAlert,
        presenting: confirmationAlertContinueAction,
        actions: { confirmationAlertContinueAction in
          Button("Cancel", role: .cancel) {
            // Nothing to do
          }
          Button("Continue", role: .destructive) {
            withErrorHandler { try await confirmationAlertContinueAction.performAction(on: self) }
          }
        },
        message: { _ in
          Text("Changing settings will sign you out and disconnect you from resources")
        }
      )
    #else
      #error("Unsupported platform")
    #endif
  }

  private var generalTab: some View {
#if os(macOS)
    VStack {
      Spacer()
      HStack {
        Spacer()
        Form {
          HStack {
            Text("Account Slug")
              .frame(width: 150, alignment: .trailing)
            TextField(
              "",
              text: $viewModel.settings.accountSlug,
              prompt: Text(PlaceholderText.accountSlug)
            )
            .disabled(viewModel.settings.isAccountSlugOverridden)
            .frame(width: 250)
          }
          .padding(.bottom, 10)

          Toggle(isOn: $viewModel.settings.connectOnStart) {
            Text("Automatically connect when Firezone is launched")
          }
          .toggleStyle(.checkbox)
          .disabled(viewModel.settings.isConnectOnStartOverridden)

          Toggle(isOn: $viewModel.settings.startOnLogin) {
            Text("Start Firezone when you sign into your Mac")
          }
          .toggleStyle(.checkbox)
          .disabled(viewModel.settings.isStartOnLoginOverridden)
        }
        .padding(10)
        Spacer()
      }
      Spacer()
    }
#elseif os(iOS)
    VStack {
      Form {
        Section(
          content: {
            VStack(alignment: .leading, spacing: 2) {
              Text("Account Slug")
                .foregroundStyle(.secondary)
                .font(.caption)
              TextField(
                PlaceholderText.accountSlug,
                text: $viewModel.settings.accountSlug
              )
              .autocorrectionDisabled()
              .textInputAutocapitalization(.never)
              .submitLabel(.done)
              .disabled(viewModel.settings.isAccountSlugOverridden)
              .padding(.bottom, 10)

              Spacer()

              Toggle(isOn: $viewModel.settings.connectOnStart) {
                Text("Automatically connect when Firezone is launched")
              }
              .toggleStyle(.switch)
              .disabled(viewModel.settings.isConnectOnStartOverridden)
            }
          },
          header: { Text("General Settings") },
        )
      }
    }
#endif
  }

  private var advancedTab: some View {
    #if os(macOS)
    VStack {
      Spacer()

      // Note
      HStack {
        Spacer()
        Text(FootnoteText.forAdvanced ?? "")
          .foregroundStyle(.secondary)
          .frame(width: 400, alignment: .trailing)
        Spacer()
      }

      Spacer()

      // Text fields
      HStack {
        Spacer()
        Form {
          // Auth Base URL
          HStack {
            Text("Auth Base URL")
              .frame(width: 150, alignment: .trailing)
            TextField(
              "",
              text: $viewModel.settings.authURL,
              prompt: Text(PlaceholderText.authBaseURL)
            )
            .disabled(viewModel.settings.isAuthURLOverridden)
            .frame(width: 250)
          }

          // API URL
          HStack {
            Text("API URL")
              .frame(width: 150, alignment: .trailing)
            TextField(
              "",
              text: $viewModel.settings.apiURL,
              prompt: Text(PlaceholderText.apiURL)
            )
            .disabled(viewModel.settings.isApiURLOverridden)
            .frame(width: 250)
          }

          // Log Filter
          HStack {
            Text("Log Filter")
              .frame(width: 150, alignment: .trailing)
            TextField(
              "",
              text: $viewModel.settings.logFilter,
              prompt: Text(PlaceholderText.logFilter)
            )
            .disabled(viewModel.settings.isLogFilterOverridden)
            .frame(width: 250)
          }
        }
        .frame(width: 500)
        Spacer()
      }

      Spacer()
    }
    #elseif os(iOS)
      VStack {
        Form {
          Section(
            content: {
              VStack(alignment: .leading, spacing: 2) {
                Text("Auth Base URL")
                  .foregroundStyle(.secondary)
                  .font(.caption)
                TextField(
                  PlaceholderText.authBaseURL,
                  text: $viewModel.settings.authURL
                )
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .submitLabel(.done)
                .disabled(viewModel.settings.isAuthURLOverridden)
              }
              VStack(alignment: .leading, spacing: 2) {
                Text("API URL")
                  .foregroundStyle(.secondary)
                  .font(.caption)
                TextField(
                  PlaceholderText.apiURL,
                  text: $viewModel.settings.apiURL
                )
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .submitLabel(.done)
                .disabled(viewModel.settings.isApiURLOverridden)
              }
              VStack(alignment: .leading, spacing: 2) {
                Text("Log Filter")
                  .foregroundStyle(.secondary)
                  .font(.caption)
                TextField(
                  PlaceholderText.logFilter,
                  text: $viewModel.settings.logFilter
                )
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .submitLabel(.done)
                .disabled(viewModel.settings.isLogFilterOverridden)
              }
              HStack {
                Spacer()
                Button(
                  "Reset to Defaults",
                  action: {
                    viewModel.reset()
                  }
                )
                .disabled(viewModel.shouldDisableResetButton)
                Spacer()
              }
            },
            header: { Text("Advanced Settings") },
            footer: { Text(FootnoteText.forAdvanced ?? "") }
          )
        }
        Spacer()
        HStack {
          Text("Build: \(BundleHelper.gitSha)")
            .textSelection(.enabled)
            .foregroundColor(.gray)
          Spacer()
        }
        .padding([.leading, .bottom], 20)
        .background(Color(uiColor: .secondarySystemBackground))
      }
      .background(Color(uiColor: .secondarySystemBackground))
    #endif
  }

  private var logsTab: some View {
    #if os(iOS)
      VStack {
        Form {
          Section(header: Text("Logs")) {
            LogDirectorySizeView(
              isProcessing: $isCalculatingLogsSize,
              sizeString: $calculatedLogsSize
            )
            .onAppear {
              self.refreshLogSize()
            }
            .onDisappear {
              self.cancelRefreshLogSize()
            }
            HStack {
              Spacer()
              ButtonWithProgress(
                systemImageName: "trash",
                title: "Clear Log Directory",
                isProcessing: $isClearingLogs,
                action: {
                  self.clearLogFiles()
                }
              )
              Spacer()
            }
          }
          Section {
            HStack {
              Spacer()
              ButtonWithProgress(
                systemImageName: "arrow.up.doc",
                title: "Export Logs",
                isProcessing: $isExportingLogs,
                action: {
                  self.isExportingLogs = true
                  Task.detached(priority: .background) {
                    let archiveURL = LogExporter.tempFile()
                    try await LogExporter.export(to: archiveURL)
                    await MainActor.run {
                      self.logTempZipFileURL = archiveURL
                      self.isPresentingExportLogShareSheet = true
                    }
                  }
                }
              )
              .sheet(isPresented: $isPresentingExportLogShareSheet) {
                if let logfileURL = self.logTempZipFileURL {
                  ShareSheetView(
                    localFileURL: logfileURL,
                    completionHandler: {
                      self.isPresentingExportLogShareSheet = false
                      self.isExportingLogs = false
                      self.logTempZipFileURL = nil
                    }
                  )
                  .onDisappear {
                    self.isPresentingExportLogShareSheet = false
                    self.isExportingLogs = false
                    self.logTempZipFileURL = nil
                  }
                }
              }
              Spacer()
            }
          }
        }
      }
    #elseif os(macOS)
      VStack {
        VStack(alignment: .leading, spacing: 10) {
          LogDirectorySizeView(
            isProcessing: $isCalculatingLogsSize,
            sizeString: $calculatedLogsSize
          )
          .onAppear {
            self.refreshLogSize()
          }
          .onDisappear {
            self.cancelRefreshLogSize()
          }
          HStack(spacing: 30) {
            ButtonWithProgress(
              systemImageName: "trash",
              title: "Clear Log Directory",
              isProcessing: $isClearingLogs,
              action: {
                self.clearLogFiles()
              }
            )
            ButtonWithProgress(
              systemImageName: "arrow.up.doc",
              title: "Export Logs",
              isProcessing: $isExportingLogs,
              action: {
                self.exportLogsWithSavePanelOnMac()
              }
            )
          }
        }
      }
    #else
      #error("Unsupported platform")
    #endif
  }

  private func saveAllSettingsAndDismiss() async throws {
    try await saveSettings()
    dismiss()
  }

  #if os(macOS)
    private func exportLogsWithSavePanelOnMac() {
      self.isExportingLogs = true

      let savePanel = NSSavePanel()
      savePanel.prompt = "Save"
      savePanel.nameFieldLabel = "Save log archive to:"
      let fileName = "firezone_logs_\(LogExporter.now()).aar"

      savePanel.nameFieldStringValue = fileName

      guard
        let window = NSApp.windows.first(where: {
          $0.identifier?.rawValue.hasPrefix("firezone-settings") ?? false
        })
      else {
        self.isExportingLogs = false
        Log.log("Settings window not found. Can't show save panel.")
        return
      }

      savePanel.beginSheetModal(for: window) { response in
        guard response == .OK else {
          self.isExportingLogs = false
          return
        }
        guard let destinationURL = savePanel.url else {
          self.isExportingLogs = false
          return
        }

        Task {
          do {
            try await LogExporter.export(
              to: destinationURL,
              with: store.ipcClient()
            )

            window.contentViewController?.presentingViewController?.dismiss(self)
          } catch {
            if let error = error as? IPCClient.Error,
               case IPCClient.Error.noIPCData = error {
              Log.warning("\(#function): Error exporting logs: \(error). Is the XPC service running?")
            } else {
              Log.error(error)
            }

            macOSAlert.show(for: error)
          }

          self.isExportingLogs = false
        }
      }
    }
  #endif

  private func refreshLogSize() {
    guard !self.isCalculatingLogsSize else {
      return
    }
    self.isCalculatingLogsSize = true
    self.calculateLogSizeTask =
    Task.detached(priority: .background) {
      let calculatedLogsSize = await calculateLogDirSize()
      await MainActor.run {
        self.calculatedLogsSize = calculatedLogsSize
        self.isCalculatingLogsSize = false
        self.calculateLogSizeTask = nil
      }
    }
  }

  private func cancelRefreshLogSize() {
    self.calculateLogSizeTask?.cancel()
  }

  private func clearLogFiles() {
    self.isClearingLogs = true
    self.cancelRefreshLogSize()
    Task.detached(priority: .background) {
      do { try await clearAllLogs() } catch { Log.error(error) }
      await MainActor.run {
        self.isClearingLogs = false
        if !self.isCalculatingLogsSize {
          self.refreshLogSize()
        }
      }
    }
  }

  private func saveSettings() async throws {
    try await viewModel.save()

    if [.connected, .connecting, .reasserting].contains(store.vpnStatus) {
      // TODO: Warn user instead of signing out
      try await self.store.signOut()
    }
  }

  // Calculates the total size of our logs by summing the size of the
  // app, tunnel, and connlib log directories.
  //
  // On iOS, SharedAccess.logFolderURL is a single folder that contains all
  // three directories, but on macOS, the app log directory lives in a different
  // Group Container than tunnel and connlib directories, so we use IPC to make
  // a call to sum both the tunnel and connlib directories.
  //
  // Unfortunately the IPC method doesn't work on iOS because the tunnel process
  // is not started on demand, so the IPC calls hang. Thus, we use separate code
  // paths for iOS and macOS.
  private func calculateLogDirSize() async -> String {
    Log.log("\(#function)")

    guard let logFilesFolderURL = SharedAccess.logFolderURL else {
      return "Unknown"
    }

    let logFolderSize = await Log.size(of: logFilesFolderURL)

    do {
#if os(macOS)
      let providerLogFolderSize = try await store.ipcClient().getLogFolderSize()
      let totalSize = logFolderSize + providerLogFolderSize
#else
      let totalSize = logFolderSize
#endif

      let byteCountFormatter = ByteCountFormatter()
      byteCountFormatter.countStyle = .file
      byteCountFormatter.allowsNonnumericFormatting = false
      byteCountFormatter.allowedUnits = [.useKB, .useMB, .useGB, .useTB, .usePB]

      return byteCountFormatter.string(fromByteCount: Int64(totalSize))

    } catch {
      if let error = error as? IPCClient.Error,
         case IPCClient.Error.noIPCData = error {
        // Will happen if the extension is not enabled
        Log.warning("\(#function): Unable to count logs: \(error). Is the XPC service running?")
      } else {
        Log.error(error)
      }

      return "Unknown"
    }
  }

  // On iOS, all the logs are stored in one directory.
  // On macOS, we need to clear logs from the app process, then call over IPC
  // to clear the provider's log directory.
  private func clearAllLogs() async throws {
    Log.log("\(#function)")

    try Log.clear(in: SharedAccess.logFolderURL)

#if os(macOS)
    try await store.clearLogs()
#endif
  }

  private func withErrorHandler(action: @escaping () async throws -> Void) {
    Task {
      do {
        try await action()
      } catch {
        Log.error(error)
#if os(iOS)
        errorHandler.handle(ErrorAlert(title: "Error performing action", error: error))
#elseif os(macOS)
        macOSAlert.show(for: error)
#endif
      }
    }
  }
}

struct ButtonWithProgress: View {
  let systemImageName: String
  let title: String
  @Binding var isProcessing: Bool
  let action: () -> Void

  var body: some View {

    VStack {
      Button(action: action) {
        Label(
          title: { Text(title) },
          icon: {
            if isProcessing {
              ProgressView().controlSize(.small)
                .frame(maxWidth: 12, maxHeight: 12)
            } else {
              Image(systemName: systemImageName)
                .frame(maxWidth: 12, maxHeight: 12)
            }
          }
        )
        .labelStyle(.titleAndIcon)
      }
      .disabled(isProcessing)
    }
    .frame(minHeight: 30)
  }
}

struct LogDirectorySizeView: View {
  @Binding var isProcessing: Bool
  @Binding var sizeString: String

  var body: some View {
    HStack(spacing: 10) {
      #if os(macOS)
        Label(
          title: { Text("Log directory size:") },
          icon: {}
        )
      #elseif os(iOS)
        Label(
          title: { Text("Log directory size:") },
          icon: {}
        )
        .foregroundColor(.secondary)
        Spacer()
      #endif
      Label(
        title: {
          if isProcessing {
            Text("")
          } else {
            Text(sizeString)
          }
        },
        icon: {
          if isProcessing {
            ProgressView().controlSize(.small)
              .frame(maxWidth: 12, maxHeight: 12)
          }
        }
      )
    }
  }
}

struct FormTextField: View {
  let title: String
  let baseURLString: String
  let placeholder: String
  let text: Binding<String>

  var body: some View {
    #if os(iOS)
      VStack(spacing: 10) {
        Spacer()
        HStack(spacing: 5) {
          Text(title)
          Spacer()
          TextField(baseURLString, text: text, prompt: Text(placeholder))
            .autocorrectionDisabled()
            .textInputAutocapitalization(.never)
        }
        Spacer()
      }
    #else
      HStack(spacing: 30) {
        Spacer()
        VStack(alignment: .leading) {
          Label(title, image: "")
            .labelStyle(.titleOnly)
            .multilineTextAlignment(.leading)
          TextField(baseURLString, text: text, prompt: Text(placeholder))
            .autocorrectionDisabled()
            .multilineTextAlignment(.leading)
            .foregroundColor(.secondary)
            .frame(maxWidth: 360)
        }
        Spacer()
      }
    #endif
  }
}

#if os(iOS)
  struct ShareSheetView: UIViewControllerRepresentable {
    let localFileURL: URL
    let completionHandler: () -> Void

    func makeUIViewController(context: Context) -> UIActivityViewController {
      let controller = UIActivityViewController(
        activityItems: [self.localFileURL],
        applicationActivities: [])
      controller.completionWithItemsHandler = { _, _, _, _ in
        self.completionHandler()
      }
      return controller
    }

    func updateUIViewController(_ uiViewController: UIActivityViewController, context: Context) {
      // Nothing to do
    }
  }
#endif
