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


  Scenario Outline: an edit to an old-style group is refused with the fix
    Given lvpm.toml has [dependencies.test] "ci_tools" "*"
    When I run "lvpm <command> --group test --prefix dest"
    Then the command fails with: [dependencies.test] is the lvpm 0.3 form of a group; rename the header to [dependency-groups.test], then run this again
    And nothing changed

    Examples:
      | command         |
      | add beta        |
      | remove ci_tools |

  Scenario: an old-style dev group is read as [dev-dependencies]
    Given lvpm.toml has [dependencies.dev] "ci_tools" "*"
    When I run "lvpm sync --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly alpha, ci_tools
    And one warning names [dev-dependencies] as the form to write

  Scenario: an edit to an old-style dev group is refused with the fix
    Given lvpm.toml has [dependencies.dev] "ci_tools" "*"
    When I run "lvpm add beta --dev --prefix dest"
    Then the command fails with: [dependencies.dev] is the lvpm 0.3 form of a group; rename the header to [dev-dependencies], then run this again
    And nothing changed
