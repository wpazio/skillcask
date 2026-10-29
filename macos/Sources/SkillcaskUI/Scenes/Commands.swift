import SkillcaskDomain
import SwiftUI

/// The menu bar (designer's note): File, Skill, Scope, View and Help, with the shortcuts, reaching the focused
/// window's stores through `FocusedValues`. View is SwiftUI's own menu with the filters, the sort order
/// and Activity inserted after its sidebar items. Add Skill…, Update…, Update All, Remove… and Adopt… set
/// `AppStore.sheet`, which `MainWindow` presents (Remove as an alert); View › Activity (⌥⌘L) shows or hides the
/// Activity drawer (S15), ticked while it shows.
public struct SkillcaskCommands: Commands {
    /// Remove…: ⌘⌫ (Remove board, "⌘⌫ from the list"); a plain ⌫ would fire while typing in a field.
    static let removeShortcut = KeyboardShortcut(.delete, modifiers: .command)

    @FocusedValue(\.appStore) private var app
    @FocusedValue(\.scopeStore) private var store
    @FocusedValue(\.actionRunner) private var runner
    @FocusedValue(\.focusSearch) private var focusSearch

    public init() {}

    /// The selected row, only in a set-up scope: a folder without skills.toml offers no skill action.
    private var selected: SkillEntry? { isSetUp ? store?.selectedEntry : nil }
    /// A sheet or the Remove alert is up: the items that would present another are disabled.
    private var isPresenting: Bool { app?.isPresenting == true }
    /// The focused scope has a skills.toml; the Skill and Scope menus act on nothing else.
    private var isSetUp: Bool {
        guard let app, let store else { return false }
        return app.isSetUp(store.scope)
    }
    /// The snapshot the Skill and Scope menus act on: none for a folder without skills.toml.
    private var snapshot: ScopeSnapshot? { isSetUp ? store?.snapshot : nil }

    public var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button(ShellCopy.openFolder) {
                if let folder = Workspace.chooseFolder(prompt: EmptyCopy.openFolder) {
                    app?.open(folder: folder)
                }
            }
            .keyboardShortcut("o")
            .disabled(app?.isReady != true)
            Button(ShellCopy.findProjects) {
                if let app, let folder = Workspace.chooseFolder(prompt: ShellCopy.findProjects) {
                    app.request(.findProjects(root: folder))
                }
            }
            .disabled(app?.isReady != true || isPresenting)
            Menu(ShellCopy.openRecent) {
                ForEach(app?.projects ?? []) { item in
                    Button(item.name) { app?.selectedScope = item.scope }
                }
            }
            .disabled(app?.isReady != true)
        }
        CommandGroup(after: .textEditing) {
            Button(ShellCopy.find) { focusSearch?.perform() }
                .keyboardShortcut("f")
                .disabled(focusSearch == nil)
        }
        CommandMenu(ShellCopy.skillMenu) {
            Button(ShellCopy.addSkill) { app?.request(.addSkill(query: nil)) }
                .keyboardShortcut("n")
                .disabled(!ToolbarRules.canAddSkill(snapshot, isBusy: isBusy) || isPresenting)
            Button(ShellCopy.update) {
                if let selected, selected.package != nil {
                    app?.request(.update(for: selected))
                }
            }
            .disabled(isPresenting || selected.map { DetailActions.updateTarget(of: $0) == nil } ?? true)
            Button(ShellCopy.updateAll) { app?.request(.updateAll) }
                .disabled(isPresenting || !ToolbarRules.canUpdateAll(snapshot))
            Divider()
            Button(ShellCopy.remove) {
                if let selected {
                    app?.request(.remove(skills: [selected.id]))
                }
            }
            .keyboardShortcut(Self.removeShortcut)
            .disabled(isPresenting || selected.map { !DetailActions.resolve($0, offline: false).showsRemove } ?? true)
            Button(ShellCopy.adopt) {
                if let selected {
                    app?.request(.adopt(skills: [selected.id]))
                }
            }
            .disabled(isPresenting || selected?.isNotManaged != true)
            Button(AdoptCopy.adoptAllMenu) {
                if let skills = AdoptPlan.batchSkills(snapshot?.entries ?? []) {
                    app?.request(.adopt(skills: skills))
                }
            }
            .disabled(isPresenting || AdoptPlan.batchSkills(snapshot?.entries ?? []) == nil)
            Divider()
            Button(ShellCopy.revealInFinder) {
                if let url = revealURL {
                    Workspace.reveal(url)
                }
            }
            .disabled(revealURL == nil)
        }
        CommandMenu(ShellCopy.scopeMenu) {
            Button(ShellCopy.sync) { start(.sync) }
                .disabled(!ToolbarRules.canSync(snapshot) || isBusy || isPresenting)
            Button(ShellCopy.check) { start(.check) }
                .disabled(snapshot == nil || isBusy || isPresenting)
            Button(ShellCopy.repair) { start(.repair) }
                .disabled(snapshot == nil || isBusy || isPresenting)
            Divider()
            // ⌘R also looks for a skills.toml again: a folder set up in Terminal shows its project after it.
            Button(ShellCopy.refresh) {
                if let store, let app {
                    Task {
                        await app.refreshSetUp()
                        guard app.isSetUp(store.scope) else { return }
                        await store.refresh()
                        await store.checkUpdates()
                    }
                }
            }
            .keyboardShortcut("r")
            .disabled(store == nil)
        }
        // SwiftUI's own View menu (Show/Hide Sidebar, Enter Full Screen) gains the filters, the sort and Activity.
        CommandGroup(after: .sidebar) {
            Divider()
            if let store {
                @Bindable var store = store
                Picker(ShellCopy.viewMenu, selection: $store.filter) {
                    ForEach(Filter.available(in: store.scope), id: \.self) { filter in
                        Text(FilterCopy.title(filter)).tag(filter)
                    }
                }
                .pickerStyle(.inline)
                .labelsHidden()
                Divider()
                Picker(ShellCopy.sortMenu, selection: $store.sort) {
                    ForEach(SkillcaskDomain.SortOrder.allCases, id: \.self) { order in
                        Text(ShellCopy.sortTitle(order)).tag(order)
                    }
                }
            }
            Divider()
            Toggle(
                ShellCopy.activity,
                isOn: Binding(get: { runner?.isActivityShown == true }, set: { runner?.isActivityShown = $0 })
            )
            .keyboardShortcut("l", modifiers: [.command, .option])
            .disabled(runner == nil)
        }
        CommandGroup(replacing: .help) {
            Link(ShellCopy.help, destination: Self.readme)
            Link(ShellCopy.readme, destination: Self.readme)
        }
    }

    /// The project README, where Skillcask Help and README both go until the app has help of its own.
    static let readme = URL(string: "https://github.com/puniserv/skillcask#readme") ?? URL(fileURLWithPath: "/")

    private var isBusy: Bool {
        guard store != nil, let runner else { return true }
        return runner.isRunning
    }

    /// Skill › Reveal in Finder: the selected row's folder, only when its path is a plain relative path that stays
    /// inside the scope root once links are resolved (`RemoveAlert.isSafe`, `staysInside`), as the Remove alert's
    /// Reveal is checked.
    private var revealURL: URL? {
        guard let store, let path = selected?.path else { return nil }
        return Self.revealURL(path: path, root: store.scope.root)
    }

    /// `root/path` when `path` is safe and the folder stays inside `root`; nil otherwise.
    nonisolated static func revealURL(path: String, root: URL) -> URL? {
        guard RemoveAlert.isSafe(path) else { return nil }
        let base = root.standardizedFileURL
        let url = base.appending(path: path).standardizedFileURL
        guard url.path.hasPrefix(base.path.hasSuffix("/") ? base.path : base.path + "/"),
            RemoveAlert.staysInside(url, root: base)
        else { return nil }
        return url
    }

    private func start(_ action: ActionRunner.Action) {
        if let store, let runner {
            runner.start(action, in: store)
        }
    }
}
