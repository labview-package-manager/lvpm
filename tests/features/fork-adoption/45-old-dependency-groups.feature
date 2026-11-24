Feature: a group written [dependencies.<group>] is still read
  labview-package-manager/lvpm#45. lvpm 0.3 documented and wrote a group as
  [dependencies.<group>]; the fork writes [dependency-groups.<group>]. The old
  form still works, with a warning that names the form to write.

  Background:
    Given fixture packages "alpha" 1.0, "beta" 1.0, "caraya" 1.0 requiring "delta" >= 1.0, "delta" 1.0, "ci_tools" 1.0 and 2.0 requiring "alpha" >= 1.0
    And a project with labview "2024" depending on "alpha" "*", with a scratch destination "dest"

  Scenario: an old-style group still installs
    Given lvpm.toml has [dependencies.test] "ci_tools" "*"
    When I run "lvpm sync --group test --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly alpha, ci_tools
    And one warning names [dependency-groups.test] as the form to write

  Scenario: the new form works without a warning
    Given lvpm.toml has [dependency-groups.test] "ci_tools" "*"
    When I run "lvpm sync --group test --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly alpha, ci_tools
    And no warning names a group

  Scenario: each old-style group is warned about once
    Given lvpm.toml has [dependencies.test] "ci_tools" "*"; [dependencies.docs] "beta" "*"
    When I run "lvpm sync --group test --group docs --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly alpha, beta, ci_tools
    And one warning names [dependency-groups.test] as the form to write
    And one warning names [dependency-groups.docs] as the form to write

  Scenario: both forms of one group are refused, naming both
    Given lvpm.toml has [dependencies.test] "ci_tools" "*"; [dependency-groups.test] "beta" "*"
    When I run "lvpm sync --group test --prefix dest"
    Then the command fails with: [dependencies.test] and [dependency-groups.test] are the same group: move its lines into [dependency-groups.test]

  Scenario Outline: an edit to an old-style group is refused with the fix
    Given lvpm.toml has [dependencies.test] "ci_tools" "*"
    When I run "lvpm <command> --group test --prefix dest"
    Then the command fails with: [dependencies.test] is the lvpm 0.3 form of a group; rename the header to [dependency-groups.test], then run this again
    And nothing changed

    Examples:
      | command         |
      | add beta        |
      | remove ci_tools |

  Scenario: an edit to another table leaves an old-style group as it is
    Given lvpm.toml has [dependencies.test] "ci_tools" "*"
    When I run "lvpm add beta --prefix dest"
    Then lvpm.toml differs from before by exactly the "beta" line
    And [dependencies] itself has the "beta" line

  Scenario: a package named like an old-style group is refused with the fix
    Given lvpm.toml has [dependencies.test] "ci_tools" "*"
    When I run "lvpm add test --prefix dest"
    Then the command fails with: [dependencies.test] is the lvpm 0.3 form of a group; rename the header to [dependency-groups.test], then run this again
    And nothing changed

  Scenario: an old-style dev group is read as [dev-dependencies]
    Given lvpm.toml has [dependencies.dev] "ci_tools" "*"
    When I run "lvpm sync --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly alpha, ci_tools
    And one warning names [dev-dependencies] as the form to write

  Scenario: --no-dev skips an old-style dev group
    Given lvpm.toml has [dependencies.dev] "ci_tools" "*"
    When I run "lvpm sync --no-dev --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly alpha

  Scenario: an edit to an old-style dev group is refused with the fix
    Given lvpm.toml has [dependencies.dev] "ci_tools" "*"
    When I run "lvpm add beta --dev --prefix dest"
    Then the command fails with: [dependencies.dev] is the lvpm 0.3 form of a group; rename the header to [dev-dependencies], then run this again
    And nothing changed

  Scenario: an upgrade that rewrites an old-style group is refused with the fix
    Given lvpm.toml has [dependencies.test] "ci_tools" "1.0" and a current lvpm.lock
    When I run "lvpm lock --upgrade-package ci_tools@2.0"
    Then the command fails with: [dependencies.test] is the lvpm 0.3 form of a group; rename the header to [dependency-groups.test], then run this again
    And nothing changed
