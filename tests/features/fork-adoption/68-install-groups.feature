Feature: pick groups at install with --with, --without, --only and include-groups
  labview-package-manager/lvpm#68. lvpm 0.3 documents --with, --without and
  --only, and lets a group take in another with include-groups. --with is
  --group without a package; --without applies after includes, and
  --without dev is --no-dev; --only installs the named groups and what they
  include, and neither [dependencies] nor dev unless named.

  Background:
    Given fixture packages "alpha" 1.0, "beta" 1.0, "caraya" 1.0 requiring "delta" >= 1.0, "delta" 1.0, "ci_tools" 1.0 and 2.0 requiring "alpha" >= 1.0
    And a project with labview "2024" depending on "alpha" "*", with a scratch destination "dest"

  Scenario Outline: the group flags decide what is installed
    Given lvpm.toml has [dev-dependencies] "caraya" "*"; [dependency-groups.test] "beta" "*"; [dependency-groups.ci] include-groups = ["test"], "ci_tools" "*"
    When I run "lvpm install <flags> --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly <installed>

    Examples:
      | flags                    | installed                           |
      | --with test              | alpha, beta, caraya, delta          |
      | --with ci                | alpha, beta, caraya, ci_tools, delta |
      | --with ci --without test | alpha, caraya, ci_tools, delta      |
      | --with ci --without dev  | alpha, beta, ci_tools               |
      | --only test              | beta                                |
      | --only ci                | alpha, beta, ci_tools               |
      | --only test,dev          | beta, caraya, delta                 |

  Scenario: an old-style group takes in another with include-groups
    Given lvpm.toml has [dependencies.test] "beta" "*"; [dependencies.ci] include-groups = ["test"], "ci_tools" "*"
    When I run "lvpm install --with ci --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly alpha, beta, ci_tools

  Scenario: the groups left out are named
    Given lvpm.toml has [dependency-groups.test] "beta" "*"; [dependency-groups.ci] "ci_tools" "*"
    When I run "lvpm install --prefix dest"
    Then the command succeeds and dest's lvpm-installed packages are exactly alpha
    And the output says "groups not selected: test, ci (--with <GROUP> adds one)"

  Scenario Outline: a wrong include-groups is refused when read
    Given lvpm.toml has <manifest>
    When I run "lvpm lock"
    Then the command fails naming <named>
    And lvpm.lock does not exist

    Examples:
      | manifest                                                                                | named                                       |
      | [dependency-groups.ci] include-groups = ["test"]                                         | "[dependency-groups.ci]" and "test"         |
      | [dependency-groups.ci] include-groups = "test"; [dependency-groups.test] "beta" "*"     | "[dependency-groups.ci]" and "include-groups" |
      | [dependency-groups.a] include-groups = ["b"]; [dependency-groups.b] include-groups = ["a"] | "include-groups" and "a -> b -> a"          |
