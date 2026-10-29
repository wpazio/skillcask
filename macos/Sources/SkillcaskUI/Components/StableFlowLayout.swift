import SwiftUI

/// Lays subviews out left to right and wraps them like `FlowLayout`, but breaks lines on each subview's width
/// plus its `flowReserve`: the list's filter chips reserve the badge a count chip may show, so for counts under 10 the
/// lines change only with the width, never when a count appears, drops to zero or goes away offline. Subviews are drawn at
/// their own widths, packed with no gaps.
///
/// It claims the whole proposed width: reporting the packed width would get it re-proposed narrower than the
/// breaks were computed for, and it would wrap again.
struct StableFlowLayout: Layout {
    let spacing: CGFloat

    /// The extra width a subview's line break counts on top of the width it is drawn at.
    struct Reserve: LayoutValueKey {
        static let defaultValue: CGFloat = 0
    }

    /// The indices of `widths` on each line: a subview goes to a new line when the current one is not empty and it
    /// would end past `available`. A subview wider than the line gets a line of its own.
    static func lines(widths: [CGFloat], available: CGFloat, spacing: CGFloat) -> [[Int]] {
        var lines: [[Int]] = []
        var lineWidth: CGFloat = 0
        for (index, width) in widths.enumerated() {
            if let last = lines.indices.last, lineWidth + spacing + width <= available {
                lines[last].append(index)
                lineWidth += spacing + width
            } else {
                lines.append([index])
                lineWidth = width
            }
        }
        return lines
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let sizes = subviews.map { $0.sizeThatFits(.unspecified) }
        let breakWidths = Self.breakWidths(sizes, subviews: subviews)
        let oneLine = breakWidths.reduce(0, +) + spacing * CGFloat(max(breakWidths.count - 1, 0))
        let width = proposal.width.flatMap { $0.isFinite ? $0 : nil } ?? oneLine
        let lines = Self.lines(widths: breakWidths, available: width, spacing: spacing)
        let heights = lines.map { line in line.map { sizes[$0].height }.max() ?? 0 }
        return CGSize(width: width, height: heights.reduce(0, +) + spacing * CGFloat(max(heights.count - 1, 0)))
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let sizes = subviews.map { $0.sizeThatFits(.unspecified) }
        let lines = Self.lines(
            widths: Self.breakWidths(sizes, subviews: subviews), available: bounds.width, spacing: spacing)
        var y = bounds.minY
        for line in lines {
            var x = bounds.minX
            for index in line {
                subviews[index].place(at: CGPoint(x: x, y: y), proposal: .unspecified)
                x += sizes[index].width + spacing
            }
            y += (line.map { sizes[$0].height }.max() ?? 0) + spacing
        }
    }

    private static func breakWidths(_ sizes: [CGSize], subviews: Subviews) -> [CGFloat] {
        zip(sizes, subviews).map { size, subview in size.width + subview[Reserve.self] }
    }
}

extension View {
    /// The extra width `StableFlowLayout` counts for this view when it breaks lines.
    func flowReserve(_ width: CGFloat) -> some View {
        layoutValue(key: StableFlowLayout.Reserve.self, value: width)
    }
}
