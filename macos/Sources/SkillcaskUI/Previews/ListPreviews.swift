import SkillcaskCore
import SkillcaskDomain
import SwiftUI

extension PreviewRegistry {
    /// The skills column at its board width over the fixture boot, one entry per S2/S11 state: List-States
    /// (empty after Set Up, filtered no matches, stale while refreshing, offline), the Components board's hover
    /// row (review-pr), Running (a Sync in the header with the success toast; the sign-in failure banner), and
    /// the Main board's selected pdf row in white on the accent; `minimumWidth` is the column at its 300 pt minimum,
    /// where the filter chips wrap to a second line.
    static let list: [Entry] = ListSample.Variant.allCases.map { variant in
        let width = variant == .minimumWidth ? Size.listMinWidth : Size.listWidth
        return Entry(id: "P3-2", state: variant.rawValue, size: CGSize(width: width, height: 800)) {
            AnyView(ListSample(variant))
        }
    }
}

/// The list column over a fresh fixture boot, so light and dark never share a store.
private struct ListSample: View {
    enum Variant: String, CaseIterable {
        case empty, noMatches, refreshing, offline, hover, running, failure, selected, minimumWidth
    }

    @State private var stores: Boot.Stores
    private let variant: Variant

    @MainActor init(_ variant: Variant) {
        self.variant = variant
        let stores = Boot.fixtures(selecting: Boot.apiScope, selection: "pdf")
        if let store = stores.app.store(for: Boot.apiScope) {
            Self.configure(variant, store: store, runner: stores.runner)
        }
        _stores = State(initialValue: stores)
    }

    @MainActor private static func configure(_ variant: Variant, store: ScopeStore, runner: ActionRunner) {
        switch variant {
        case .empty:
            store.preload(snapshot: DesignFixtures.emptyAfterSetUp)
        case .noMatches:
            store.query = DesignFixtures.noMatchesQuery
            store.filter = .updates
            store.selection = nil
        case .refreshing:
            store.preload(snapshot: DesignFixtures.api, selection: "jira-communication", isRefreshing: true)
        case .offline:
            store.preload(snapshot: DesignFixtures.apiOffline, presentation: .offline)
        case .hover, .selected, .minimumWidth:
            break
        case .running:
            runner.preloadRunning(title: ActionCopy.syncing, in: Boot.apiScope)
            runner.toast = ToastMessage(text: DesignFixtures.updatedToast.message, emphasis: "pdf")
        case .failure:
            store.selection = DesignFixtures.sqlReview.id
            let banner = DesignFixtures.signInBanner
            runner.outcome = .banner(ErrorCopy.message(for: banner.error, title: banner.title), id: UUID())
        }
    }

    var body: some View {
        if let store = stores.app.store(for: Boot.apiScope) {
            SkillListView(store: store, hoveredRow: variant == .hover ? DesignFixtures.reviewPr.id : nil)
                // A render is one frame: the toast is drawn already presented, not mid-way through its insertion.
                .transaction { $0.disablesAnimations = true }
                .skillcaskEnvironment(
                    app: stores.app, runner: stores.runner, settings: stores.settings, client: stores.client)
        }
    }
}
