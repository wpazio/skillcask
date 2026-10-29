import Foundation
import SkillcaskCore
import Testing

@testable import SkillcaskDomain

/// Which filters a scope offers (D38: "Not managed" only in the global scope, "Project skills" only in a project)
/// and where a filter the scope lacks lands.
@Suite struct FilterAvailabilityTests {
    let project = Scope.project(URL(fileURLWithPath: "/tmp/filter-tests"))

    @Test func mySkillsOffersNotManagedButNoProjectSkills() {
        #expect(Filter.available(in: .personal) == [.all, .needsAttention, .updates, .notManaged])
    }

    @Test func aProjectOffersProjectSkillsButNoNotManaged() {
        #expect(Filter.available(in: project) == [.all, .needsAttention, .updates, .projectSkills])
    }

    @Test func everyScopeOffersFourFiltersStartingWithAll() {
        for scope in [Scope.personal, project] {
            let filters = Filter.available(in: scope)
            #expect(filters.count == 4)
            #expect(filters.first == .all)
        }
    }

    @Test func aFilterTheScopeLacksFallsBackToAll() {
        #expect(Filter.projectSkills.resolved(in: .personal) == .all)
        #expect(Filter.notManaged.resolved(in: project) == .all)
        for scope in [Scope.personal, project] {
            for filter in Filter.available(in: scope) {
                #expect(filter.resolved(in: scope) == filter)
            }
        }
    }
}
