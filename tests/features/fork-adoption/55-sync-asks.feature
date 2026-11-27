Feature: sync asks before it removes packages
  labview-package-manager/lvpm#55.

  Background:
    Given fixture packages "alpha" 1.0, "alpha" 2.0, "extra" 1.0 and "zeta" 1.0
    And a project whose lvpm.toml says labview = ">=2026"
    And the project depends on "alpha" ">=1.0"
    And the project has no lvpm.lock

  Scenario Outline: sync asks before removing, and "no" changes nothing
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    When I run "lvpm sync --prefix dest" in a terminal and answer "<answer>"
    Then it asked "remove 1 package (extra 1.0)? [y/N]"
    And the command succeeds
    And the output says "sync cancelled: nothing was changed"
    And the output says "next: add it to lvpm.toml with lvpm add extra, or run lvpm sync --prefix dest --yes"
    And dest's lvpm-installed packages are exactly "alpha" 1.0 and "extra" 1.0
    And the project has no lvpm.lock

    Examples:
      | answer    |
      | n         |
      | no        |
      |           |
      | yes please |

  Scenario: a closed stdin is "no"
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    When I run "lvpm sync --prefix dest" in a terminal with stdin closed
    Then it asked "[y/N]"
    And dest's lvpm-installed packages are exactly "alpha" 1.0 and "extra" 1.0

  Scenario: "n" leaves an existing lvpm.lock byte-for-byte
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    And the project has an lvpm.lock locking only "alpha" 1.0
    When I run "lvpm sync --prefix dest" in a terminal and answer "n"
    Then the project's lvpm.lock is unchanged
    And dest's lvpm-installed packages are exactly "alpha" 1.0 and "extra" 1.0

  Scenario Outline: "y" goes ahead
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    When I run "lvpm sync --prefix dest" in a terminal and answer "<answer>"
    Then it asked "[y/N]"
    And the command succeeds
    And dest's lvpm-installed packages are exactly "alpha" 2.0
    And the project has an lvpm.lock

    Examples:
      | answer |
      | y      |
      | Y      |
      | yes    |

  Scenario: the question names every package it would remove
    Given dest has "alpha" 1.0, "extra" 1.0 and "zeta" 1.0 installed
    When I run "lvpm sync --prefix dest" in a terminal and answer "n"
    Then it asked "remove 2 packages (extra 1.0, zeta 1.0)? [y/N]"

  Scenario Outline: --yes and LVPM_YES answer the question
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    And LVPM_YES is set to "<env>"
    When I run "<command>" in a terminal
    Then it did not ask "[y/N]"
    And dest's lvpm-installed packages are exactly "alpha" 2.0

    Examples:
      | command                        | env |
      | lvpm sync --prefix dest --yes  |     |
      | lvpm sync --prefix dest        | 1   |

  Scenario: a falsey LVPM_YES still asks
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    And LVPM_YES is set to "0"
    When I run "lvpm sync --prefix dest" in a terminal and answer "n"
    Then it asked "[y/N]"

  Scenario Outline: nothing to remove, nothing to ask
    Given <dest>
    When I run "lvpm sync --prefix dest" in a terminal
    Then it did not ask "[y/N]"
    And the command succeeds
    And dest's lvpm-installed packages are exactly "alpha" 2.0

    Examples:
      | dest                                |
      | an empty dest                       |
      | dest has "alpha" 1.0 installed      |
      | dest has "alpha" 2.0 installed      |

  Scenario: a bare install asks the same way
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    When I run "lvpm install --prefix dest" in a terminal and answer "n"
    Then it asked "[y/N]"
    And dest's lvpm-installed packages are exactly "alpha" 1.0 and "extra" 1.0

  Scenario: with no terminal, a prefix goes ahead
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    When I run "lvpm sync --prefix dest" with no terminal
    Then it did not ask "[y/N]"
    And the output names "extra" as a package to remove
    And dest's lvpm-installed packages are exactly "alpha" 2.0

  Scenario: --json in a terminal does not ask
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    When I run "lvpm sync --prefix dest --json" in a terminal
    Then it did not ask "[y/N]"
    And dest's lvpm-installed packages are exactly "alpha" 2.0

  Scenario Outline: named commands do not ask
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    When I run "<command>" in a terminal
    Then it did not ask "[y/N]"
    And <outcome>

    Examples:
      | command                                  | outcome                                                   |
      | lvpm install zeta --prefix dest          | dest's lvpm-installed packages are exactly "alpha" 2.0 and "zeta" 1.0 |
      | lvpm uninstall alpha --prefix dest       | dest has no lvpm-installed packages                       |
      | lvpm add zeta --prefix dest              | the manifest depends on "zeta"                             |
      | lvpm remove alpha --prefix dest          | the manifest no longer depends on "alpha"                  |

  Scenario: --dry-run lists and asks nothing
    Given dest has "alpha" 1.0 and "extra" 1.0 installed
    When I run "lvpm sync --prefix dest --dry-run" in a terminal
    Then it did not ask "[y/N]"
    And the output names "extra" as a package to remove
    And dest's lvpm-installed packages are exactly "alpha" 1.0 and "extra" 1.0

  Scenario: a dev-only removal does not ask
    Given the project dev-depends on "zeta" ">=1.0"
    And dest has "alpha" 2.0 and "zeta" 1.0 installed
    When I run "lvpm sync --prefix dest --no-dev" in a terminal
    Then it did not ask "[y/N]"
    And dest's lvpm-installed packages are exactly "alpha" 2.0
