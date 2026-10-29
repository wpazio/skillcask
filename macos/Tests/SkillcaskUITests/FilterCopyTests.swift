import SkillcaskDomain
import SwiftUI
import Testing

@testable import SkillcaskUI

/// The chip titles, the badge counts and colours, the reach-group copy and visibility rule, and the source
/// pill names, each pinned to the board's text. Parsing CLI source strings is Domain's (`SourceKindTests`).
@Suite struct FilterCopyTests {
    @Test func everyFilterHasTheBoardTitle() {
        let expected: [Filter: String] = [
            .all: "All",
            .needsAttention: "Needs attention",
            .updates: "Updates",
            .notManaged: "Not managed",
            .projectSkills: "Project skills",
        ]
        for filter in Filter.allCases {
            #expect(FilterCopy.title(filter) == expected[filter])
        }
        #expect(Set(Filter.allCases.map(FilterCopy.title)).count == Filter.allCases.count)
    }

    /// The list's chips carry short titles; the full title stays everywhere else (View menu, Overview, empty
    /// states) and is the chip's tooltip and accessibility label.
    @Test func listChipsUseShortTitles() {
        let expected: [Filter: String] = [
            .all: "All",
            .needsAttention: "Attention",
            .updates: "Updates",
            .notManaged: "Unmanaged",
            .projectSkills: "Project",
        ]
        for filter in Filter.allCases {
            #expect(FilterCopy.chipTitle(filter) == expected[filter])
        }
        #expect(Set(Filter.allCases.map(FilterCopy.chipTitle)).count == Filter.allCases.count)
        #expect(FilterCopy.barLabel == "Filter")
    }

    @Test func badgeTextHidesNilAndZero() {
        #expect(FilterChip.countText(nil) == nil)
        #expect(FilterChip.countText(0) == nil)
        #expect(FilterChip.countText(5) == "5")
        #expect(FilterChip.countText(12) == "12")
    }

    @Test func barCountsComeFromAttentionInTheBoardColours() {
        let attention = Attention(errors: 3, warnings: 5, updates: 3)
        let counts = FilterChipBar.counts(for: attention)
        #expect(counts == [.needsAttention: 8, .updates: 3])
        #expect(FilterChipBar.countSeverity(.needsAttention) == .error)
        #expect(FilterChipBar.countSeverity(.updates) == .update)
        for filter in [Filter.all, .notManaged, .projectSkills] {
            #expect(FilterChipBar.countSeverity(filter) == .neutral)
            #expect(counts[filter] == nil)
        }
    }

    @Test func reachGroupsUseTheBoardLabelsAndHideWhenEmpty() {
        let labels = ReachCopy.Group.allCases.map(ReachCopy.label)
        #expect(labels == ["Commands", "Hosts", "Paths", "Environment variables", "Scripts"])
        #expect(ReachCopy.title("Commands", count: 3) == "Commands · 3")
        let commands = ChipGroup.Model(label: ReachCopy.label(.commands), items: ["pdftotext", "qpdf", "python3"])
        #expect(!commands.isHidden)
        #expect(commands.title == "Commands · 3")
        let hosts = ChipGroup.Model(label: ReachCopy.label(.network), items: ["pypi.org"])
        #expect(hosts.title == "Hosts · 1")
        let env = ChipGroup.Model(label: ReachCopy.label(.env), items: [])
        #expect(env.isHidden)
    }

    @Test func sourcePillsUseTheBoardLabelsAndGlyphs() {
        let cases: [(SourceKind, String, String?)] = [
            (.packagist, "Packagist", "shippingbox"),
            (.github, "GitHub", "circle.fill"),
            (.git, "Git", "arrow.triangle.branch"),
            (.skillsSh, "skills.sh", "globe"),
            (.custom("acme"), "acme", "rectangle"),
            (.project, "Project", nil),
            (.local, "Local", nil),
        ]
        for (source, label, symbol) in cases {
            #expect(SourceCopy.label(source) == label, "\(source)")
            #expect(Pill.symbol(of: source) == symbol, "\(source)")
        }
        #expect(SourceCopy.blocked(.github) == "GitHub · blocked")
        #expect(Pill.style(of: .project) == .plain)
        #expect(Pill.style(of: .github) == .source(symbol: "circle.fill"))
    }

    /// The idle chip's hairline is a filled ring inside the chip's frame: its edge is painted, its middle and
    /// everything outside are not, so nothing reaches past the capsule.
    @Test func chipHairlineIsARingInsideTheFrame() {
        let rect = CGRect(x: 0, y: 0, width: 80, height: 22)
        let ring = CapsuleRing(width: 1).path(in: rect)
        // The straight top and bottom edges only: how far the round ends reach at mid-height depends on how the
        // SDK flattens `Capsule`'s curves (the macOS 26 SDK on CI differs from Xcode 27 here).
        #expect(ring.contains(CGPoint(x: 40, y: 0.5), eoFill: true))
        #expect(ring.contains(CGPoint(x: 40, y: 21.5), eoFill: true))
        #expect(!ring.contains(CGPoint(x: 40, y: 11), eoFill: true))
        #expect(!ring.contains(CGPoint(x: 40, y: 2), eoFill: true))
        #expect(!ring.contains(CGPoint(x: 0.5, y: 0.5), eoFill: true))
        #expect(ring.boundingRect == rect)
    }
}
