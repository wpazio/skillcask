import SkillcaskCore

/// The filter chips above the list (brief IA). `apply` also takes the search field's text.
public enum Filter: String, CaseIterable, Hashable, Sendable {
    case all
    case needsAttention
    case updates
    case notManaged
    case projectSkills

    /// The filters a scope offers, in chip order: "Not managed" only in the global scope (D38), "Project skills"
    /// only in a project. The list's chips and the View menu both show this list.
    public static func available(in scope: Scope) -> [Filter] {
        scope.isPersonal
            ? [.all, .needsAttention, .updates, .notManaged] : [.all, .needsAttention, .updates, .projectSkills]
    }

    /// This filter where `scope` offers it, else All: a filter set for another scope never leaves an empty list
    /// behind a chip that is not drawn.
    public func resolved(in scope: Scope) -> Filter {
        Self.available(in: scope).contains(self) ? self : .all
    }

    public func matches(_ entry: SkillEntry) -> Bool {
        switch self {
        case .all: true
        case .needsAttention: entry.health.severity >= .warning
        case .updates: entry.hasUpdate
        case .notManaged: entry.isNotManaged
        case .projectSkills: entry.isProjectSkill || entry.kind == .handWritten
        }
    }

    /// Entries matching the chip and the query (name, package or description, case-insensitive), in `sort` order
    /// (worst first, then by name, unless the list's sort menu says otherwise).
    public func apply(_ entries: [SkillEntry], query: String = "", sort: SortOrder = .state) -> [SkillEntry] {
        let needle = query.trimmingCharacters(in: .whitespaces).lowercased()
        return sort.apply(entries.filter { matches($0) && (needle.isEmpty || $0.searchText.contains(needle)) })
    }
}

extension SkillEntry {
    var searchText: String {
        [name, package ?? "", description ?? ""].joined(separator: " ").lowercased()
    }
}
