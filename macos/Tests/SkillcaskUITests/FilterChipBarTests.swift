import Foundation
import SkillcaskCore
import SkillcaskDomain
import SwiftUI
import Testing

@testable import SkillcaskUI

/// The list's filter bar (S2): the scope's four chips, the stable line breaks of `StableFlowLayout` and the
/// store falling back to All for a filter the scope lacks.
@Suite @MainActor struct FilterChipBarTests {
    let project = Scope.project(URL(fileURLWithPath: "/tmp/filter-chip-bar-tests"))
    let capabilities = Capabilities(version: Capabilities.minimum)

    // MARK: - Line breaks

    @Test func noChipsMakeNoLines() {
        #expect(StableFlowLayout.lines(widths: [], available: 300, spacing: 6) == [])
    }

    @Test func chipsThatFitExactlyStayOnOneLine() {
        #expect(StableFlowLayout.lines(widths: [50, 50, 50], available: 162, spacing: 6) == [[0, 1, 2]])
        #expect(StableFlowLayout.lines(widths: [50, 50, 50], available: 161.5, spacing: 6) == [[0, 1], [2]])
    }

    @Test func aChipWiderThanTheLineGetsALineOfItsOwn() {
        #expect(StableFlowLayout.lines(widths: [400], available: 300, spacing: 6) == [[0]])
        #expect(StableFlowLayout.lines(widths: [40, 400, 40], available: 300, spacing: 6) == [[0], [1], [2]])
    }

    /// Chip widths at 10 pt padding with every count badge shown (one digit: 16 pt badge plus 4 pt gap), as the P2-3
    /// render draws them: All, Attention, Updates, then Unmanaged (My Skills, 326 pt) or Project (a project, 297 pt).
    static let mySkills: [CGFloat] = [35, 94, 89, 90]
    static let projectChips: [CGFloat] = [35, 94, 89, 61]

    /// Usable widths: the 300 pt minimum, the 360 pt ideal and a 480 pt list column, less 12 pt padding a side.
    @Test(arguments: [(CGFloat(276), 2), (336, 1), (456, 1)])
    func fourChipsWrapOnlyAtTheMinimumColumn(available: CGFloat, lineCount: Int) {
        let spacing = Spacing.inline
        #expect(
            StableFlowLayout.lines(widths: Self.mySkills, available: available, spacing: spacing).count == lineCount)
        #expect(
            StableFlowLayout.lines(widths: Self.projectChips, available: available, spacing: spacing).count
                == lineCount)
    }

    @Test func theMinimumColumnMovesOnlyTheLastChipDown() {
        #expect(StableFlowLayout.lines(widths: Self.mySkills, available: 276, spacing: 6) == [[0, 1, 2], [3]])
    }

    // MARK: - Stable breaks

    /// A count chip without its badge reserves the badge's width; one with a badge, or a chip that never counts,
    /// reserves nothing.
    @Test func onlyCountChipsWithoutABadgeReserveTheBadge() {
        let badge = FilterChip.badgeReserve
        #expect(badge == Size.badgeSmall + Spacing.xs)
        #expect(FilterChipBar.reserve(for: .needsAttention, counts: [:]) == badge)
        #expect(FilterChipBar.reserve(for: .updates, counts: [.updates: 0]) == badge)
        #expect(FilterChipBar.reserve(for: .updates, counts: [.updates: 3]) == 0)
        for filter in [Filter.all, .notManaged, .projectSkills] {
            #expect(FilterChipBar.reserve(for: filter, counts: [:]) == 0)
        }
    }

    /// Counts appearing, dropping to zero or Updates' count going offline never move a chip to another line.
    @Test(arguments: [CGFloat(276), 300, 336, 456])
    func breaksAreTheSameWithAndWithoutBadges(available: CGFloat) {
        let filters = Filter.available(in: .personal)
        let withBadges = Self.mySkills
        let badge = FilterChip.badgeReserve
        let withoutBadges = zip(filters, withBadges)
            .map { filter, width in
                FilterChipBar.countSeverity(filter) == .neutral ? width : width - badge
            }
        let full: [Filter: Int] = [.needsAttention: 5, .updates: 2]
        let countSets: [[Filter: Int]] = [full, [:], [.needsAttention: 0, .updates: 0], [.needsAttention: 5]]
        let expected = StableFlowLayout.lines(widths: withBadges, available: available, spacing: 6)
        for counts in countSets {
            let drawn = zip(filters, zip(withBadges, withoutBadges))
                .map { filter, widths in
                    (FilterChip.countText(counts[filter]) == nil) ? widths.1 : widths.0
                }
            let breakWidths = zip(filters, drawn)
                .map { filter, width in
                    width + FilterChipBar.reserve(for: filter, counts: counts)
                }
            #expect(StableFlowLayout.lines(widths: breakWidths, available: available, spacing: 6) == expected)
        }
    }

    // MARK: - The store's filter

    @Test func aStoreFallsBackToAllForAFilterItsScopeLacks() {
        let personal = ScopeStore(scope: .personal, client: FakeCLIClient(), capabilities: capabilities)
        personal.filter = .projectSkills
        #expect(personal.filter == .all)
        personal.filter = .notManaged
        #expect(personal.filter == .notManaged)

        let store = ScopeStore(scope: project, client: FakeCLIClient(), capabilities: capabilities)
        store.filter = .updates
        store.filter = .notManaged
        #expect(store.filter == .all)
        store.filter = .projectSkills
        #expect(store.filter == .projectSkills)
    }
}
