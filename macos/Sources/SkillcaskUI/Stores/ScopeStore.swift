import Foundation
import Observation
import SkillcaskCore
import SkillcaskDomain

/// Everything the window knows about one scope: the documents of the last refresh as a
/// `ScopeSnapshot`, the list's filter, query and selection, and the selected row's `SkillDetail`. One store per
/// scope, cached by `AppStore`, never shared, writing only to itself, so no result of one scope can land in another.
///
/// Refresh policy: opening runs the offline trio (`status`, `audit`, `doctor`) in parallel; `outdated` runs only
/// on an explicit `checkUpdates()`. Every write after an `await` is guarded by the generation captured at the
/// start and by `Task.isCancelled`, so a late answer is dropped. Documents are replaced, never nilled
/// first: the previous snapshot stays on screen while `isRefreshing`. Selection is an id re-resolved against the
/// new snapshot; a vanished row deselects and the Overview shows.
@MainActor @Observable public final class ScopeStore {
    /// Reads a skill's SKILL.md off the main actor; injected so previews and tests need no files on disk.
    public typealias TextReader = @Sendable (URL) async throws -> SkillText

    /// A scope re-selected within this interval is not refreshed again.
    public static let freshness: TimeInterval = 60

    public let scope: Scope
    /// The CLI seam this store (and `ActionRunner`, for this scope) talks through.
    public let client: any CLIClient
    /// What the located CLI can do; every `Invocation` of this scope carries it.
    public let capabilities: Capabilities
    private let textReader: TextReader
    /// Settings read on open (`checkUpdatesOnOpen`); nil in tests that need none.
    private let settings: SettingsStore?
    /// The app's queue, asked before an automatic refresh; nil in tests that need none.
    private let queue: MutationQueue?
    /// Where the reads the user starts are recorded for Activity (S15), set by `AppStore`; nil records nothing.
    /// Recorded: opening the scope (`refreshIfStale` when it reads: "Opened api"), `refresh()` (⌘R, a failed
    /// Overview's Retry: "Refreshed api") and `checkUpdates()` (⌘R, Check Now: "Checked for updates"). Not
    /// recorded, because nobody asked for them: the refresh a finished mutation runs (`refreshAfterMutation`; the
    /// mutation's own row stands for it) and the `outdated` that Settings "check updates when a project opens"
    /// adds to the first open.
    @ObservationIgnored public var recordActivity: ActionRunner.ActivitySink?
    /// Called when a refresh of a project finds no skills.toml in its folder (the CLI's or `GuardedClient`'s
    /// `skills.toml not found`), so `AppStore` checks the sidebar's set-up state again; nil in tests that need none.
    @ObservationIgnored public var onManifestMissing: (@MainActor () -> Void)?

    /// Why a read runs, which decides its Activity row.
    private enum Read {
        case open
        case refresh
        case checkUpdates
    }

    // MARK: - State

    public private(set) var snapshot: ScopeSnapshot?
    public private(set) var presentation: Presentation = .loading
    /// The selected row's id (`SkillEntry.ID`); bound to the list's selection.
    public var selection: SkillEntry.ID?
    /// The active filter chip; a filter the scope does not offer (`Filter.available(in:)`) is All.
    public var filter: Filter {
        get { selectedFilter }
        set { selectedFilter = newValue.resolved(in: scope) }
    }
    private var selectedFilter: Filter = .all
    /// The list's sort menu; the state order is the default (S2 note).
    public var sort: SkillcaskDomain.SortOrder = .state
    /// The search field's text.
    public var query = ""
    /// The offline trio is in flight: the header progress and the list fade.
    public private(set) var isRefreshing = false
    /// `outdated` is in flight: Check Now is disabled, the strip may say so.
    public private(set) var isCheckingUpdates = false
    /// When the documents were last read, whatever the trigger.
    public private(set) var checkedAt: Date?
    /// The last failure of a read-only command; kept beside the snapshot so the header can say what went wrong.
    public private(set) var lastError: CLIError?
    /// The last refresh read `status` but `audit` or `doctor` failed: the findings or Health on screen are the
    /// previous ones (or missing on a first load), and the Overview header says so. Cleared by a refresh in which
    /// both answered.
    public private(set) var partialFailure: PartialFailure?

    /// Which of `audit` and `doctor` failed in the last refresh, and whether an earlier document stands in.
    public struct PartialFailure: Hashable, Sendable {
        public enum Read: Hashable, Sendable {
            case audit
            case doctor
        }

        /// The failed reads, `audit` first.
        public let failed: [Read]
        /// The failed reads whose earlier document is still shown.
        public let kept: Set<Read>
        /// The first failure, for Details.
        public let error: CLIError

        public init(failed: [Read], kept: Set<Read>, error: CLIError) {
            self.failed = failed
            self.kept = kept
            self.error = error
        }
    }
    public private(set) var detail: SkillDetail?
    public private(set) var isLoadingDetail = false

    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var checkGeneration = 0
    @ObservationIgnored private var detailGeneration = 0
    @ObservationIgnored private var documents = Documents()

    /// The raw documents behind `snapshot`, kept so a check can add `outdated` without re-reading the trio.
    private struct Documents {
        var status: StatusDocument?
        var audit: AuditDocument?
        var doctor: DoctorDocument?
        var outdated: OutdatedDocument?
        /// The last Repair's `doctor --fix --json`: what it fixed and what it left. Only the refresh the Repair
        /// itself starts (`refreshAfterMutation(repair:)`) sets it, when that refresh lands; any other refresh that
        /// lands, and that one failing, clears it. So the Health popover says what was repaired (S12 "After
        /// Repair") until the next Check, ⌘R or re-open, and a ⌘R racing the Repair's refresh wins.
        var repair: DoctorDocument?
    }

    public init(
        scope: Scope, client: any CLIClient, capabilities: Capabilities, settings: SettingsStore? = nil,
        queue: MutationQueue? = nil, textReader: @escaping TextReader = ScopeStore.readText
    ) {
        self.scope = scope
        self.client = client
        self.capabilities = capabilities
        self.settings = settings
        self.queue = queue
        self.textReader = textReader
    }

    // MARK: - Derived

    /// The rows the list shows: the chip's filter, the search field's query, the sort menu's order.
    public var visibleEntries: [SkillEntry] {
        filter.apply(snapshot?.entries ?? [], query: query, sort: sort)
    }

    /// The entry of `selection` in the current snapshot; nil when nothing is selected or the row vanished.
    public var selectedEntry: SkillEntry? {
        snapshot?.entry(id: selection)
    }

    /// The last network command failed: what only `outdated` knows is hidden.
    public var isOffline: Bool {
        presentation == .offline
    }

    /// The skills that require a dependency row (`ScopeSnapshot.requirerSkills(of:)`).
    public func requirerSkills(for entry: SkillEntry) -> [String] {
        snapshot?.requirerSkills(of: entry) ?? []
    }

    // MARK: - Refresh

    /// The offline trio unless the snapshot is younger than `freshness`: what a scope re-selection runs. Deferred
    /// while a mutation of this scope runs or waits (the runner refreshes when it finishes). With
    /// Settings "check updates when a project opens", the first open also runs `outdated` in the background.
    public func refreshIfStale() async {
        if let checkedAt, snapshot != nil, Date().timeIntervalSince(checkedAt) < Self.freshness {
            return
        }
        if let queue, await queue.isBusy(scope) {
            return
        }
        let firstOpen = snapshot == nil
        await refresh(keeping: nil, recording: .open)
        if firstOpen, snapshot != nil, settings?.checkUpdatesOnOpen == true {
            await checkUpdates(recording: nil)
        }
    }

    /// `status`, `audit` and `doctor` in parallel, then one snapshot (on the first load a status-only snapshot
    /// first, as soon as `status` answers). `outdated` is kept from the last check.
    /// `isRefreshing` ends with this refresh however it returns (cancelled included), unless a newer one started.
    public func refresh() async {
        await refresh(keeping: nil, recording: .refresh)
    }

    /// `refresh()`, with the Repair document `repair` shown in place of `doctor` when this refresh lands, and the
    /// Activity row of `read` (none for a mutation's refresh).
    private func refresh(keeping repair: DoctorDocument?, recording read: Read?) async {
        let startedAt = Date()
        generation += 1
        let mine = generation
        isRefreshing = true
        defer {
            if mine == generation {
                isRefreshing = false
            }
        }
        async let status = traced(StatusDocument.self, .status)
        async let audit = traced(AuditDocument.self, .audit)
        async let doctor = traced(DoctorDocument.self, .doctor(fix: false))
        let (statusResult, statusTrace) = await status
        // Flow-F1 "Rows fill in as each answers": on the first load the rows show from `status` alone while
        // `audit` and `doctor` still run; they merge below. A later refresh keeps the previous snapshot instead, so
        // findings and Health never blink out (and a Repair's document keeps its lifetime).
        if snapshot == nil, mine == generation, !Task.isCancelled, case .success(let document) = statusResult {
            documents.status = document
            rebuild()
        }
        let ((auditResult, auditTrace), (doctorResult, doctorTrace)) = await (audit, doctor)
        // A read superseded or cancelled (switching scopes cancels the list's `.task(id:)`) is not the user's
        // outcome: only a read that ended is recorded, with the first failure among the three.
        guard mine == generation, !Task.isCancelled else { return }
        if let read {
            let failures = [
                (Self.failure(of: statusResult), statusTrace), (Self.failure(of: auditResult), auditTrace),
                (Self.failure(of: doctorResult), doctorTrace),
            ]
            let failed = failures.first { $0.0 != nil }
            record(
                read, startedAt: startedAt, traces: [statusTrace, auditTrace, doctorTrace], error: failed?.0,
                status: failed?.1.status)
        }
        switch statusResult {
        case .success(let document):
            documents.status = document
            // A failed `audit` or `doctor` keeps the document of the last refresh in which it answered, so findings,
            // Health and the SKILL.md paths do not vanish behind a passing failure; `partialFailure` says so.
            var failed: [PartialFailure.Read] = []
            var kept: Set<PartialFailure.Read> = []
            var firstError: CLIError?
            switch auditResult {
            case .success(let audit):
                documents.audit = audit
            case .failure(let error):
                if error != .cancelled {
                    failed.append(.audit)
                    firstError = firstError ?? error
                    if documents.audit != nil { kept.insert(.audit) }
                }
            }
            switch doctorResult {
            case .success(let doctor):
                documents.doctor = doctor
            case .failure(let error):
                if error != .cancelled {
                    failed.append(.doctor)
                    firstError = firstError ?? error
                    if documents.doctor != nil || repair != nil { kept.insert(.doctor) }
                }
            }
            documents.repair = repair
            lastError = nil
            partialFailure = firstError.map { PartialFailure(failed: failed, kept: kept, error: $0) }
            rebuild()
        case .failure(.cancelled):
            documents.repair = nil
        case .failure(let error):
            documents.repair = nil
            lastError = error
            // The whole refresh failed: `lastError` says why, and no strip about `audit` or `doctor` from an older
            // refresh stays beside it.
            partialFailure = nil
            if !scope.isPersonal, Presentation.forFailedLoad(error) == .noManifest {
                // The folder has no skills.toml (any more): nothing read before stays on screen, and the sidebar's
                // set-up state is checked again, so the window shows Set Up instead of a stale or parent project.
                forgetDocuments()
                presentation = .noManifest
                onManifestMissing?()
            } else if snapshot == nil {
                presentation = Presentation.forFailedLoad(error)
            }
        }
    }

    /// Drops every document, the snapshot and the selection: the scope has no project to show.
    private func forgetDocuments() {
        documents = Documents()
        snapshot = nil
        partialFailure = nil
        selection = nil
        detail = nil
    }

    /// `outdated --json`: the one network read, never implicit (Check Now, ⌘R, the Updates card, or Settings
    /// "check on open"). Its own generation, so a check never drops an in-flight trio and a newer check drops an
    /// older one; the new document joins whatever trio documents are current when it lands.
    public func checkUpdates() async {
        await checkUpdates(recording: .checkUpdates)
    }

    /// `checkUpdates()`, recorded in Activity only when `read` says so.
    private func checkUpdates(recording read: Read?) async {
        let startedAt = Date()
        checkGeneration += 1
        let mine = checkGeneration
        isCheckingUpdates = true
        defer {
            if mine == checkGeneration {
                isCheckingUpdates = false
            }
        }
        let (outcome, trace) = await traced(OutdatedDocument.self, .outdated)
        guard mine == checkGeneration, !Task.isCancelled else { return }
        if let read {
            record(read, startedAt: startedAt, traces: [trace], error: Self.failure(of: outcome))
        }
        switch outcome {
        case .success(let document):
            documents.outdated = document
            lastError = nil
            rebuild()
        case .failure(.cancelled):
            break
        case .failure(let error):
            lastError = error
            if snapshot != nil, case .failed(.network, _, _, _) = error {
                presentation = .offline
            } else if snapshot == nil {
                presentation = Presentation.forFailedLoad(error)
            }
        }
    }

    /// The refresh a finished mutation asks for: the offline trio, and `outdated` again only when it had
    /// been checked before, so "last checked" stays truthful.
    /// The selected row's detail is read again after the trio, so its info and Contents show what the mutation
    /// installed (an update's new version) instead of what was there before.
    /// - Parameter repair: The finished Repair's `doctor --fix --json`, shown until the next refresh lands.
    public func refreshAfterMutation(repair: DoctorDocument? = nil) async {
        await refresh(keeping: repair, recording: nil)
        if selectedEntry != nil {
            await loadDetail()
        }
        if documents.outdated != nil {
            await checkUpdates(recording: nil)
        }
    }

    /// Previews and tests: the documents and selection of a scope without running anything. The snapshot is
    /// rebuilt from `documents` by every later refresh, so a seeded `ScopeSnapshot` is shown as given.
    /// `presentation` and `isRefreshing` let a preview show the list offline or stale while refreshing.
    public func preload(
        snapshot: ScopeSnapshot, selection: SkillEntry.ID? = nil, detail: SkillDetail? = nil,
        presentation: Presentation = .ready, isRefreshing: Bool = false
    ) {
        self.snapshot = snapshot
        self.selection = selection
        self.detail = detail
        // Counted from now, not from the snapshot's own date: a seeded scope is fresh, so opening it does not start
        // a refresh that would fade the list (the strip still shows the snapshot's "Checked 4 min ago").
        checkedAt = Date()
        self.presentation = presentation
        self.isRefreshing = isRefreshing
    }

    /// Previews: the warning of a refresh that read `status` but not `audit` or `doctor`.
    func preload(partialFailure: PartialFailure) {
        self.partialFailure = partialFailure
    }

    private func rebuild() {
        guard let status = documents.status else { return }
        let built = ScopeSnapshot(
            status: status, audit: documents.audit, doctor: documents.repair ?? documents.doctor,
            outdated: documents.outdated)
        snapshot = built
        checkedAt = built.checkedAt
        presentation = .ready
        if selection != nil, built.entry(id: selection) == nil {
            selection = nil
            detail = nil
        }
    }

    private func result<T: Decodable & Sendable>(
        _ type: T.Type, _ command: Invocation.Command
    ) async -> Result<T, CLIError> {
        await traced(type, command).0
    }

    /// `result`, with what the read left for its Activity row (`CLIClient.document` is `run` then `document`).
    private func traced<T: Decodable & Sendable>(
        _ type: T.Type, _ command: Invocation.Command
    ) async -> (Result<T, CLIError>, ActionRunner.ReadTrace) {
        var trace = ActionRunner.ReadTrace(command: ActionCopy.unbuilt)
        do {
            let invocation = try Invocation(command, in: scope, capabilities: capabilities)
            trace.command = invocation.displayCommand
            let run = try await client.run(invocation)
            trace.status = run.status
            trace.stderr = run.stderr
            trace.environmentOverrides = run.environmentOverrides
            return (.success(try run.document(type)), trace)
        } catch {
            return (.failure(error), trace)
        }
    }

    private static func failure<T>(of result: Result<T, CLIError>) -> CLIError? {
        if case .failure(let error) = result { error } else { nil }
    }

    /// Hands the row of `read` to `recordActivity`.
    /// A read that ended in `.cancelled` was stopped by the app (the scope changed), not by the user: no row.
    /// `status` is the failing command's exit, the first trace's otherwise.
    private func record(
        _ read: Read, startedAt: Date, traces: [ActionRunner.ReadTrace], error: CLIError?,
        status: ExitStatus? = nil
    ) {
        guard let recordActivity, error != .cancelled else { return }
        let name = ShellCopy.name(of: scope)
        let (headline, failure): (String, String) =
            switch read {
            case .open: (ActivityCopy.opened(name), ActivityCopy.couldNotOpen(name))
            case .refresh: (ActivityCopy.refreshed(name), ActivityCopy.couldNotRefresh(name))
            case .checkUpdates: (ActivityCopy.checkedForUpdates, ActivityCopy.couldNotCheckForUpdates)
            }
        recordActivity(
            .read(
                headline: headline, failure: failure, scope: scope, startedAt: startedAt, traces: traces, error: error,
                status: status))
    }

    // MARK: - Selection

    /// Selects `id` and loads its detail; `nil` shows the Overview.
    public func select(_ id: SkillEntry.ID?) async {
        selection = id
        await loadDetail()
    }

    /// Esc or a click on empty detail space (S3): no row is selected, the detail drops, the Scope Overview shows.
    public func clearSelection() {
        selection = nil
        detail = nil
    }

    /// `info --json -- <skill>` (offline, milliseconds) and the SKILL.md, in parallel, under the detail generation:
    /// what `.task(id: store.selection)` runs. A not managed row has no info document; a row without a path has
    /// no file.
    public func loadDetail() async {
        detailGeneration += 1
        let mine = detailGeneration
        guard let entry = selectedEntry else {
            detail = nil
            isLoadingDetail = false
            return
        }
        if detail != nil, loadedDetailID != entry.id {
            detail = nil
        }
        isLoadingDetail = true
        async let info = infoResult(for: entry)
        async let text = textResult(for: entry)
        let (infoResult, textResult) = await (info, text)
        guard mine == detailGeneration, !Task.isCancelled else { return }
        isLoadingDetail = false
        loadedDetailID = entry.id
        var infoError: CLIError?
        if case .failure(let error) = infoResult {
            infoError = error
        }
        detail = SkillDetail(
            info: try? infoResult.get(), text: textResult.text, infoError: infoError, textError: textResult.error)
    }

    @ObservationIgnored private var loadedDetailID: SkillEntry.ID?

    private func infoResult(for entry: SkillEntry) async -> Result<InfoDocument?, CLIError> {
        guard !entry.isNotManaged else { return .success(nil) }
        return await result(InfoDocument.self, .info(.skill(entry.id))).map { Optional($0) }
    }

    private func textResult(for entry: SkillEntry) async -> (text: SkillText?, error: String?) {
        guard let path = entry.path else { return (nil, nil) }
        let url = scope.root.appending(path: path).appending(path: "SKILL.md")
        do {
            return (try await textReader(url), nil)
        } catch {
            return (nil, ContentsCopy.readError(error))
        }
    }

    // MARK: - Package versions

    /// `info --json -- <package>` per package, kept for the session: the Versions tab's full list.
    public private(set) var packageInfo: [String: InfoDocument] = [:]
    /// Packages whose `info` is in flight.
    public private(set) var loadingPackageInfo: Set<String> = []
    /// Why the last `info <package>` failed; cleared when it is asked again.
    public private(set) var packageInfoErrors: [String: CLIError] = [:]

    /// The network `info --json -- <package>` behind the Versions tab's "All known versions": run only when the
    /// list is expanded, never on selection; answered from the session cache when it was loaded before.
    /// Cancelling the calling task (collapsing the list, leaving the row) cancels the command; a cancelled load
    /// records no error, so expanding again retries.
    ///
    /// A call while one is in flight runs nothing of its own: it waits for that run and, when the run leaves
    /// neither a document nor an error (it was cancelled: the list collapsed and opened again before the command
    /// returned), asks itself, so a re-expanded list never stays on a spinner nobody drives.
    public func loadPackageInfo(_ package: String) async {
        while packageInfo[package] == nil, !Task.isCancelled {
            guard loadingPackageInfo.contains(package) else {
                await runPackageInfo(package)
                return
            }
            await waitForPackageInfo(package)
            if packageInfoErrors[package] != nil {
                return
            }
        }
    }

    @ObservationIgnored private var packageInfoWaiters: [String: [UUID: CheckedContinuation<Void, Never>]] = [:]

    private func runPackageInfo(_ package: String) async {
        loadingPackageInfo.insert(package)
        packageInfoErrors[package] = nil
        let outcome = await result(InfoDocument.self, .info(.package(package)))
        loadingPackageInfo.remove(package)
        switch outcome {
        case .success(let document):
            packageInfo[package] = document
        case .failure(.cancelled):
            break
        case .failure(let error):
            if !Task.isCancelled {
                packageInfoErrors[package] = error
            }
        }
        let waiters = packageInfoWaiters.removeValue(forKey: package) ?? [:]
        for waiter in waiters.values {
            waiter.resume()
        }
    }

    /// Suspends until the in-flight run for `package` ends, or the caller is cancelled.
    private func waitForPackageInfo(_ package: String) async {
        let id = UUID()
        await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                guard !Task.isCancelled, loadingPackageInfo.contains(package) else {
                    continuation.resume()
                    return
                }
                packageInfoWaiters[package, default: [:]][id] = continuation
            }
        } onCancel: {
            Task { @MainActor in self.resumeWaiter(id, of: package) }
        }
    }

    private func resumeWaiter(_ id: UUID, of package: String) {
        packageInfoWaiters[package]?.removeValue(forKey: id)?.resume()
    }

    /// The default reader: a nonisolated async function runs off the caller's actor (tools 6.0), so the file read
    /// never blocks the main actor.
    public nonisolated static func readText(at url: URL) async throws -> SkillText {
        try SkillText.read(contentsOf: url)
    }
}
