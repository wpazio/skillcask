import SkillcaskDomain

/// The titles of the filters, verbatim from the Components board ("Filter chips and buttons"), and the short
/// titles the list's chips draw in the narrow column. Views never spell these; `FilterChipBar` and the View menu
/// read them here.
public enum FilterCopy {
    /// The full title of a filter: the View menu, the Overview, empty states, and a chip's tooltip and
    /// accessibility label.
    public static func title(_ filter: Filter) -> String {
        switch filter {
        case .all: "All"
        case .needsAttention: "Needs attention"
        case .updates: "Updates"
        case .notManaged: "Not managed"
        case .projectSkills: "Project skills"
        }
    }

    /// The short title a list chip draws, so the scope's four chips fit one line of the 360 pt list column.
    public static func chipTitle(_ filter: Filter) -> String {
        switch filter {
        case .all: "All"
        case .needsAttention: "Attention"
        case .updates: "Updates"
        case .notManaged: "Unmanaged"
        case .projectSkills: "Project"
        }
    }

    /// What VoiceOver calls the list's chip bar.
    public static let barLabel = "Filter"
}
