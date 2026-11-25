Feature: install and uninstall are the project commands
  labview-package-manager/lvpm#54. In a project, "lvpm install <pkg>" edits
  lvpm.toml, locks and syncs; the destination flags (--prefix, --global,
  --labview-version) only pick where the project syncs. "--no-project" installs
  directly and leaves the project alone, as in uv.

  Background:
    Given fixture packages "alpha" 1.0, "app" 1.0 requiring "lib" >= 1.0, "app2" 1.0 requiring "lib" >= 1.0, "lib" 1.0, "caraya" 1.0
    And a project with labview "2024" depending on "alpha" "*", locked and synced into scratch destination "dest"

  # Every command here that reaches a destination names one: a project with no
  # venv has none of its own, and the sandbox never writes into a LabVIEW.
  Scenario: a named install edits the project
    When I run "lvpm install caraya --prefix dest"
    Then lvpm.toml says "caraya" = ">=1.0"
    And lvpm.lock and dest include "caraya" 1.0
    And the output says "adding caraya to lvpm.toml"

  Scenario: a destination flag picks where the project syncs, and install still edits it
    When I run "lvpm install caraya --prefix other"
    Then lvpm.toml says "caraya" = ">=1.0"
    And lvpm.lock and "other" include "caraya" 1.0 and "alpha" 1.0
    And the output says "in a project, --prefix picks where the project syncs" and names "--no-project"

  Scenario: --no-project installs without touching the project
    When I run "lvpm install caraya --no-project --prefix dest"
    Then dest includes "caraya" 1.0
    And lvpm.toml and lvpm.lock are byte-identical to before

  Scenario: --no-project takes the options a lock cannot honour
    When I run "lvpm install app --no-project --no-deps --prefix dest"
    Then dest includes "app" 1.0
    And dest does not include "lib"
    And lvpm.toml and lvpm.lock are byte-identical to before

  Scenario Outline: options a lock cannot honour name --no-project
    When I run "lvpm install caraya <flag>"
    Then the command fails saying <flag> cannot be used in a project, because lvpm.lock decides the dependencies
    And the error's next line names "--no-project"
    And nothing changed

    Examples:
      | flag              |
      | --no-deps         |
      | --manifest x.toml |

  Scenario: an unknown package changes nothing
    When I run "lvpm install zeta --prefix dest"
    Then the command fails saying "zeta" was not found
    And nothing changed

  Scenario Outline: --no-project needs a package, and is not --project
    When I run "lvpm <args>"
    Then the command fails naming "<names>"
    And nothing changed

    Examples:
      | args                                    | names     |
      | install --no-project                    | <PACKAGE> |
      | install caraya --no-project --project . | --project |

  Scenario: --no-project outside a project is a direct install, as without it
    Given a directory without lvpm.toml
    When I run "lvpm install caraya --no-project --prefix dest"
    Then dest includes "caraya" 1.0
