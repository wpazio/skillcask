import SkillcaskDomain
import SwiftUI

/// The Add Skill header's source filter: "All" with the total, then one `FilterChip` per `SearchStore.sourceChips`
/// with its glyph and count, single-select on `sourceFilter`, scrolling sideways (the Skills list's `FilterChipBar`
/// wraps instead). A source with no results is dimmed and disabled unless it is the active one, so the filter can
/// always be cleared; clicking the active chip goes back to All. Unlike the Skills list, which dims for offline and
/// keeps its chips selectable, a dimmed chip here would only show an empty list. A repository the policy blocks follows the chips as a plain,
/// non-interactive pill: its results count only in All.
struct SourceFilterBar: View {
    let search: SearchStore

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: Spacing.s) {
                chips
                ForEach(search.repositories.filter { !$0.allowed }, id: \.name) { repository in
                    Pill(AddSkillCopy.blockedByPolicy(SourceCopy.label(repository.kind)))
                        .opacity(ResultsColumn.blockedOpacity)
                }
            }
        }
    }

    private var chips: some View {
        HStack(spacing: Spacing.inline) {
            let total = search.totalResultCount
            FilterChip(FilterCopy.title(.all), count: total, isActive: search.sourceFilter == nil) {
                search.sourceFilter = nil
            }
            .accessibilityLabel(FilterCopy.title(.all))
            .accessibilityValue(Self.accessibilityValue(total))
            ForEach(search.sourceChips, id: \.name) { chip in
                let label = SourceCopy.label(chip.kind)
                let isActive = Self.isActive(chip, filter: search.sourceFilter)
                let isEnabled = Self.isEnabled(chip, isActive: isActive)
                FilterChip(label, symbol: Pill.symbol(of: chip.kind), count: chip.count, isActive: isActive) {
                    search.sourceFilter = Self.filter(afterTapping: chip, current: search.sourceFilter)
                }
                .opacity(isEnabled ? 1 : FilterChipBar.dimmedOpacity)
                .disabled(!isEnabled)
                .accessibilityLabel(label)
                .accessibilityValue(Self.accessibilityValue(chip.count))
                .accessibilityHint(AddSkillCopy.filterHint(label))
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(AddSkillCopy.filterLabel)
    }

    // MARK: - Derived

    /// The chip is the filter's: the same name ignoring case (the store matches sources so).
    static func isActive(_ chip: SearchStore.SourceChip, filter: String?) -> Bool {
        guard let filter else { return false }
        return chip.name.caseInsensitiveCompare(filter) == .orderedSame
    }

    /// A chip can be clicked when its source has results, or when it is active (to clear it).
    static func isEnabled(_ chip: SearchStore.SourceChip, isActive: Bool) -> Bool {
        chip.count > 0 || isActive
    }

    /// The filter a click leaves: the chip's source, or All when the chip was already active.
    static func filter(afterTapping chip: SearchStore.SourceChip, current: String?) -> String? {
        isActive(chip, filter: current) ? nil : chip.name
    }

    /// What VoiceOver reads after a chip's label: `20 results`, `1 result`.
    static func accessibilityValue(_ count: Int) -> String {
        AddSkillCopy.results(shown: count, total: count)
    }

    /// The empty filter's source as the chips name it; the raw filter when no chip matches it.
    static func label(of filter: String, in chips: [SearchStore.SourceChip]) -> String {
        chips.first { isActive($0, filter: filter) }.map { SourceCopy.label($0.kind) }
            ?? SourceCopy.label(SourceKind(parsing: filter))
    }
}
