Feature: project.labview is required, and a bare year is read as a minimum
  labview-package-manager/lvpm#44. lvpm 0.3 documented labview = "2026"; the
  fork wants labview = ">=2026". A bare year still works, with a warning that
  names the form to write.

  Background:
    Given fixture packages "alpha" 1.0 and 2.0, "beta" 1.0 requiring "alpha" >= 1.0, "gamma" 1.0 and 1.5, "caraya" 1.0

  Scenario: a manifest without labview is refused with the fix
    Given a project manifest without labview, with a scratch destination "dest"
    And no lvpm.lock exists
    When I run "lvpm sync --prefix dest"
    Then the command fails with: lvpm.toml has no labview
    And the command fails with: add labview = ">=<year>" under [project] in lvpm.toml
    And nothing changed

  Scenario: a bare year is a minimum
    Given a project whose lvpm.toml says labview = "2026"
    And the project depends on "beta" ">=1.0"
    When I run "lvpm lock"
    Then the command succeeds
    And lvpm.lock records labview "2026"
    And one warning names labview = ">=2026" as the form to write

  Scenario: a command that reads the manifest more than once warns once
    Given a project whose lvpm.toml says labview = "2026"
    And the project depends on "beta" ">=1.0"
    And an empty dest
    And no lvpm.lock exists
    When I run "lvpm sync --prefix dest"
    Then the command succeeds
    And one warning names labview = ">=2026" as the form to write

  Scenario: the explicit form gives no warning
    Given a project whose lvpm.toml says labview = ">=2026"
    And the project depends on "beta" ">=1.0"
    When I run "lvpm lock"
    Then the command succeeds
    And lvpm.lock records labview "2026"
    And no warning names labview
