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
    And dest is byte-identical to before
    And the output says "in a project, --prefix picks where the project syncs" and names "--no-project"

  Scenario Outline: --dry-run says what it would do and changes nothing
    When I run "lvpm <args>"
    Then the output says "would"
    And nothing changed

    Examples:
      | args                                    |
      | install caraya --dry-run --prefix dest  |
      | uninstall alpha --dry-run --prefix dest |

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
      | --allow-downgrade |
      | --manifest x.toml |

  Scenario: an unknown package changes nothing
    When I run "lvpm install zeta --prefix dest"
    Then the command fails saying "zeta" is in no configured source
    And the output does not say "adding"
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

  # Slice 2: uninstall. As above, a command that reaches a destination names one.
  Scenario: uninstall takes the package out of the project
    When I run "lvpm uninstall alpha --prefix dest"
    Then "alpha" is gone from lvpm.toml, lvpm.lock and dest
    And the output says "removing alpha from lvpm.toml"

  Scenario: uninstall drops what only that package needed
    Given the project also depends on "app" "*", locked and synced
    When I run "lvpm uninstall app --prefix dest"
    Then "app" is gone from lvpm.toml, lvpm.lock and dest
    And "lib" is gone from lvpm.lock and dest

  Scenario: uninstall keeps what another package still needs
    Given the project also depends on "app" "*", locked and synced
    And the project also depends on "app2" "*", locked and synced
    When I run "lvpm uninstall app --prefix dest"
    Then "app" is gone from lvpm.toml, lvpm.lock and dest
    And "lib" stays in lvpm.lock and dest

  Scenario: a destination flag picks where uninstall syncs
    Given the project is also synced into scratch destination "other"
    When I run "lvpm uninstall alpha --prefix other"
    Then "alpha" is gone from lvpm.toml, lvpm.lock and "other"
    And dest still includes "alpha" 1.0
    And the output says "in a project, --prefix picks where the project syncs" and names "--no-project"

  Scenario: --with-deps is accepted in a project
    When I run "lvpm uninstall alpha --with-deps --prefix dest"
    Then "alpha" is gone from lvpm.toml, lvpm.lock and dest

  Scenario: uninstall --dev takes a dev dependency out
    Given dev dependency "caraya" ">=1.0", locked and synced
    When I run "lvpm uninstall caraya --dev --prefix dest"
    Then "caraya" is gone from [dev-dependencies], lvpm.lock and dest

  Scenario: uninstall --group takes a package out of that group
    Given lvpm.toml has [dependency-groups.test] "caraya" "*", a current lvpm.lock, and dest synced with "--group test"
    When I run "lvpm uninstall caraya --group test --prefix dest"
    Then "caraya" is gone from [dependency-groups.test], lvpm.lock and dest

  Scenario Outline: uninstall from the wrong table names the right one
    Given <given>
    When I run "lvpm uninstall caraya <flag> --prefix dest"
    Then the command fails saying "caraya" is in <table>
    And the error's next line names "lvpm uninstall caraya"
    And the error's next line names "--prefix dest"
    And the output does not say "removing"
    And nothing changed

    Examples:
      | given                                                                                                      | flag       | table                    |
      | dev dependency "caraya" ">=1.0", locked and synced                                                         |            | [dev-dependencies]       |
      | lvpm.toml has [dependency-groups.test] "caraya" "*", a current lvpm.lock, and dest synced with "--group test" | --dev      | [dependency-groups.test] |
      | the project also depends on "caraya" "*", locked and synced                                                | --dev      | [dependencies]           |

  Scenario: a package listed in two tables comes out of one
    Given the project also depends on "caraya" "*", locked and synced
    And dev dependency "caraya" ">=1.0", locked and synced
    When I run "lvpm uninstall caraya --dev --prefix dest"
    Then "caraya" is gone from [dev-dependencies]
    And "caraya" stays in lvpm.lock and dest

  Scenario: uninstall of an installed name the project does not list names --no-project
    Given "caraya" 1.0 is installed in dest with "lvpm install caraya --no-project --prefix dest"
    When I run "lvpm uninstall caraya --prefix dest"
    Then the command fails with: caraya is not a dependency of this project
    And the error's next line names "lvpm uninstall caraya --no-project --prefix dest"
    And the output does not say "removing"
    And nothing changed

  Scenario: uninstall of a name that is nowhere suggests a typo
    When I run "lvpm uninstall zeta --prefix dest"
    Then the command fails with: zeta is not a dependency of this project
    And the error's next line says to check the name
    And the error does not mention "--no-project"
    And the output does not say "removing"
    And nothing changed

  Scenario: uninstall --group of a group the project does not define names it and changes nothing
    When I run "lvpm uninstall caraya --group ci --prefix dest"
    Then the command fails naming "unknown group 'ci'"
    And the output does not say "removing"
    And nothing changed

  Scenario Outline: flags the lock overrules are refused in a project
    When I run "lvpm uninstall alpha <flag>"
    Then the command fails naming "<flag>"
    And the error's next line names "--no-project"
    And nothing changed

    Examples:
      | flag        |
      | --keep-deps |
      | --force     |

  Scenario: uninstall --no-project needs a package
    When I run "lvpm uninstall --no-project"
    Then the command fails naming "<PACKAGE>"
    And nothing changed

  Scenario Outline: --no-project and --all keep uninstall direct
    When I run "lvpm uninstall <args>"
    Then "alpha" is uninstalled from dest
    And lvpm.toml and lvpm.lock are byte-identical to before

    Examples:
      | args                                         |
      | alpha --no-project --prefix dest             |
      | alpha --no-project --keep-deps --prefix dest |
      | --all --prefix dest                          |

  Scenario: a bare uninstall in a project stays direct
    When I run "lvpm uninstall --prefix dest"
    Then the command fails with: give a package name, or --all
    And lvpm.toml and lvpm.lock are byte-identical to before
    And nothing changed

  Scenario Outline: project-only flags on a direct uninstall are refused
    When I run "lvpm uninstall <args>"
    Then the command fails naming "<flag>"
    And nothing changed

    Examples:
      | args                                          | flag         |
      | alpha --dev --no-project --prefix dest        | --dev        |
      | alpha --group test --no-project --prefix dest | --group      |
      | alpha --dry-run --no-project --prefix dest    | --dry-run    |
      | alpha --no-linkfix --no-project --prefix dest | --no-linkfix |
      | --all --dry-run --prefix dest                 | --dry-run    |

  Scenario: a direct uninstall of a listed package says it is still listed
    When I run "lvpm uninstall alpha --no-project --prefix dest"
    Then the output says "alpha" is still in lvpm.toml and "lvpm uninstall alpha" removes it for good

  # Slice 3: a package another one requires. Each refusal names the roots that
  # require it, and the destination as typed, so the command it suggests acts
  # on the same target.
  Scenario: uninstall of a package another one requires, not listed itself
    Given the project also depends on "app" "*", locked and synced
    When I run "lvpm uninstall lib --prefix dest"
    Then the command fails with: lib is required by installed packages: app
    And the error's next line says to run "lvpm uninstall app" first
    And the error's next line names "--prefix dest"
    And the output does not say "removing"
    And nothing changed

  Scenario: uninstall of a listed package another one requires
    Given the project also depends on "app" "*", locked and synced
    And the project also depends on "lib" "*", locked and synced
    When I run "lvpm uninstall lib --prefix dest"
    Then the command fails with: lib is required by installed packages: app
    And the error's next line says to run "lvpm uninstall app" first
    And the output does not say "removing"
    And nothing changed

  Scenario: two packages require it
    Given the project also depends on "app" "*", locked and synced
    And the project also depends on "app2" "*", locked and synced
    When I run "lvpm uninstall lib --prefix dest"
    Then the command fails with: lib is required by installed packages: app, app2
    And the error's next line says to run "lvpm uninstall app" and "lvpm uninstall app2" first
    And nothing changed

  Scenario: --dry-run refuses it the same way
    Given the project also depends on "app" "*", locked and synced
    And the project also depends on "lib" "*", locked and synced
    When I run "lvpm uninstall lib --dry-run --prefix dest"
    Then the command fails with: lib is required by installed packages: app
    And the error's next line says to run "lvpm uninstall app" first
    And nothing changed

  Scenario: following the next line works
    Given the project also depends on "app" "*", locked and synced
    And the project also depends on "lib" "*", locked and synced
    When I run "lvpm uninstall app --prefix dest"
    And I run "lvpm uninstall lib --prefix dest"
    Then "app" and "lib" are gone from lvpm.toml, lvpm.lock and dest

  Scenario: a root in [dev-dependencies] is named with --dev, and following the next line works
    Given dev dependency "app" "*", locked and synced
    When I run "lvpm uninstall lib --prefix dest"
    Then the command fails with: lib is required by installed packages: app
    And the error's next line says to run "lvpm uninstall app --dev" first
    And the error's next line names "--dev --prefix dest"
    And nothing changed
    When I run "lvpm uninstall app --dev --prefix dest"
    Then "app" is gone from [dev-dependencies], lvpm.lock and dest

  Scenario: the direct route says it the same way
    Given the project also depends on "app" "*", locked and synced
    When I run "lvpm uninstall lib --no-project --prefix dest"
    Then the command fails with: lib is required by installed packages: app
    And the error's next line says to run "lvpm uninstall app" first
    And the error's next line names "--no-project --prefix dest"
    And the error's next line names "--force"
    And nothing changed

  # Slice 4: the saving flags. As above, a command that reaches a destination names one.
  Scenario: install can save into the dev group
    When I run "lvpm install caraya --dev --prefix dest"
    Then [dev-dependencies] says "caraya" = ">=1.0"
    And lvpm.lock and dest include "caraya" 1.0

  Scenario: install --group saves into that group
    When I run "lvpm install caraya --group test --prefix dest"
    Then [dependency-groups.test] says "caraya" = ">=1.0"
    And the output says "created [dependency-groups.test], a new group"
    And lvpm.lock and dest include "caraya" 1.0

  Scenario: install --dev with a destination flag saves and syncs there
    When I run "lvpm install caraya --dev --prefix other"
    Then [dev-dependencies] says "caraya" = ">=1.0"
    And "other" includes "caraya" 1.0

  Scenario: install --dev of a package [dependencies] lists adds it there too
    Given the project also depends on "caraya" "*", locked and synced
    When I run "lvpm install caraya --dev --prefix dest"
    Then [dev-dependencies] says "caraya" = ">=1.0"
    And [dependencies] itself has the "caraya" line

  Scenario: a bare install --group still installs that group
    Given lvpm.toml has [dependency-groups.test] "caraya" "*" and a current lvpm.lock
    When I run "lvpm install --group test --prefix dest"
    Then lvpm.lock and dest include "caraya" 1.0
    And lvpm.toml is byte-identical to before

  Scenario Outline: saving flags that do not fit are refused
    When I run "lvpm install <args> --prefix dest"
    Then the command fails naming "<names>"
    And nothing changed

    Examples:
      | args                               | names        |
      | caraya --group test --group ci     | one group    |
      | caraya --dev --group test          | --dev        |
      | caraya --dev --no-dev              | --dev        |
      | --dev                              | --dev        |
      | --index second                     | --index      |
      | caraya --dev --no-project          | --no-project |
      | caraya --group test --no-project   | --no-project |
      | caraya --index second --no-project | --no-project |

  Scenario Outline: saving flags are refused outside a project too, naming the project
    Given a directory without lvpm.toml
    When I run "lvpm install caraya <flag> --prefix dest"
    Then the command fails naming "<names>"
    And the error's next line names "run this in a project"
    And the working directory has no lvpm.toml

    Examples:
      | flag                 | names             |
      | --dev                | --dev             |
      | --group test         | --group           |
      | --index second       | --index           |
      | --dev --index second | --dev and --index |

  # Slice 5: add and remove are the same commands as install and uninstall.
  # Where they reach a destination they name one, as above.
  Scenario: add does what install does
    When I run "lvpm add caraya --dev --prefix other", and "lvpm install caraya --dev --prefix other" in an identical world
    Then both manifests, locks, destinations and outputs are identical

  Scenario: remove does what uninstall does
    Given dev dependency "caraya" ">=1.0", locked and synced
    When I run "lvpm remove caraya --dev --prefix dest", and "lvpm uninstall caraya --dev --prefix dest" in an identical world
    Then both manifests, locks, destinations and outputs are identical

  Scenario: add outside a project installs directly, as install does
    Given a directory without lvpm.toml
    When I run "lvpm add caraya --prefix dest"
    Then dest includes "caraya" 1.0
    And the working directory has no lvpm.toml

  Scenario: remove outside a project uninstalls directly, as uninstall does
    Given a directory without lvpm.toml
    When I run "lvpm remove alpha --prefix dest"
    Then "alpha" is uninstalled from dest
    And the working directory has no lvpm.toml

  Scenario: remove takes uninstall's direct flags
    When I run "lvpm remove alpha --no-project --prefix dest"
    Then "alpha" is uninstalled from dest
    And lvpm.toml and lvpm.lock are byte-identical to before

  Scenario Outline: the aliases report the command they stand for
    When I run "lvpm --json <alias> <package> --prefix dest"
    Then the JSON result is {"command": "<command>"}

    Examples:
      | alias  | package | command   |
      | add    | caraya  | install   |
      | remove | alpha   | uninstall |

  Scenario: help lists the aliases
    When I run "lvpm --help"
    Then the install line names alias "add" and the uninstall line names alias "remove"
