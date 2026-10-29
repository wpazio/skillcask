import SkillcaskCore
import SkillcaskDomain
import SwiftUI

/// The segmented filter above the list: one `FilterChip` per filter the scope offers (`Filter.available(in:)`),
/// exactly one active. Chips draw the short `FilterCopy.chipTitle` at 10 pt padding; the full title is the tooltip
/// and the accessibility label. "Attention" counts in the error colour, "Updates" in the update colour, the rest
/// carry no badge. A dimmed chip (List-States offline: "Updates" at 50 %) stays selectable but reads as unavailable.
///
/// The chips wrap rather than scroll or hide (`StableFlowLayout`), with line breaks computed as if every count chip
/// showed a one-digit badge: below 10 the number of lines changes only with the column's width, never with the counts.
public struct FilterChipBar: View {
    public let filters: [Filter]
    public let counts: [Filter: Int]
    public let dimmed: Set<Filter>
    @Binding public var selection: Filter

    /// List-States offline draws the Updates chip at `opacity: 0.5`.
    public static let dimmedOpacity: Double = 0.5

    /// The chips' side padding: 10 pt rather than the board's 12, so four chips fit the 360 pt list column.
    public static let chipPadding: CGFloat = 10

    public init(
        filters: [Filter], counts: [Filter: Int] = [:], dimmed: Set<Filter> = [], selection: Binding<Filter>
    ) {
        self.filters = filters
        self.counts = counts
        self.dimmed = dimmed
        _selection = selection
    }

    /// The bar for a scope: counts from its `Attention` (rows needing a decision, rows with updates).
    public init(filters: [Filter], attention: Attention, selection: Binding<Filter>) {
        self.init(filters: filters, counts: Self.counts(for: attention), selection: selection)
    }

    /// The chip counts an `Attention` yields: "Needs attention" is red plus amber, "Updates" the update count.
    nonisolated public static func counts(for attention: Attention) -> [Filter: Int] {
        [.needsAttention: attention.needing, .updates: attention.updates]
    }

    /// The badge colour of a filter's count.
    nonisolated public static func countSeverity(_ filter: Filter) -> Severity {
        switch filter {
        case .needsAttention: .error
        case .updates: .update
        case .all, .notManaged, .projectSkills: .neutral
        }
    }

    /// The width a chip's line break counts beyond its drawn width: a count chip without a badge reserves one, so
    /// a count appearing or vanishing never moves a chip to another line.
    nonisolated public static func reserve(for filter: Filter, counts: [Filter: Int]) -> CGFloat {
        countSeverity(filter) != .neutral && FilterChip.countText(counts[filter]) == nil ? FilterChip.badgeReserve : 0
    }

    public var body: some View {
        StableFlowLayout(spacing: Spacing.inline) {
            ForEach(filters, id: \.self) { filter in
                chip(filter)
                    .flowReserve(Self.reserve(for: filter, counts: counts))
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(FilterCopy.barLabel)
    }

    private func chip(_ filter: Filter) -> some View {
        let title = FilterCopy.title(filter)
        let count = counts[filter]
        let severity = Self.countSeverity(filter)
        return FilterChip(
            FilterCopy.chipTitle(filter), count: count, countSeverity: severity, isActive: selection == filter,
            horizontalPadding: Self.chipPadding
        ) {
            selection = filter
        }
        .opacity(dimmed.contains(filter) ? Self.dimmedOpacity : 1)
        .help(title)
        .accessibilityLabel(title)
        .accessibilityValue(Self.accessibilityValue(count: count, severity: severity))
    }

    /// The chip's spoken count: the badge's label, or nothing when the chip draws no badge.
    nonisolated static func accessibilityValue(count: Int?, severity: Severity) -> String {
        guard let count, count > 0 else { return "" }
        return CountBadge.accessibilityLabel(count: count, severity: severity)
    }
}

/// Holds the selection the previews and the render tool bind to.
struct FilterChipBarSample: View {
    @State private var selection: Filter
    private let scope: Scope
    private let attention: Attention

    init(selection: Filter, scope: Scope = .personal, attention: Attention) {
        _selection = State(initialValue: selection)
        self.scope = scope
        self.attention = attention
    }

    var body: some View {
        FilterChipBar(filters: Filter.available(in: scope), attention: attention, selection: $selection)
    }
}

#Preview("FilterChipBar, light") {
    FilterChipBarSample(selection: .all, attention: Attention(errors: 3, warnings: 2, updates: 2))
        .padding(Spacing.l)
        .background(Color.surfaceWindow)
}

#Preview("FilterChipBar, dark") {
    FilterChipBarSample(selection: .needsAttention, attention: Attention(errors: 3, warnings: 2, updates: 2))
        .padding(Spacing.l)
        .background(Color.surfaceWindow)
        .preferredColorScheme(.dark)
}
