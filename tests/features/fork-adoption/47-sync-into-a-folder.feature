Feature: a project is installed into a folder with lvpm sync --prefix
  labview-package-manager/lvpm#47. lvpm 0.3 installed a project into a folder
  with "lvpm install --manifest <FILE> --prefix <DIR>". The fork drops
  install's --manifest, --no-save and --no-hooks; "lvpm sync --prefix <DIR>"
  takes the place of the first.

  Background:
    Given fixture packages "alpha" 1.0 and 2.0, "beta" 1.0 requiring "alpha" >= 1.0, "gamma" 1.0 and 1.5, "caraya" 1.0

  Scenario: a project is installed into a folder
    Given a project whose lvpm.toml says labview = ">=2026"
    And the project depends on "alpha" ">=1.0"
    And an empty dest
    When I run "lvpm sync --prefix dest"
    Then the command succeeds
    And alpha's files are under dest
