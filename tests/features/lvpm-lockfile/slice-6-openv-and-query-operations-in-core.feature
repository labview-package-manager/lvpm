Feature: Destination, index and queries live in core

  Scenario Outline: query output is unchanged
    When the characterization row "<name>" runs
    Then it still matches

    Examples:
      | name           |
      | search-a       |
      | search-none    |
      | list-beta      |
      | list-empty     |
      | flags-conflict |
      | config-set     |
      | config-get     |
      | cache-add      |

  # Lists the real LabVIEW 2026 installation, so it needs one.
  @labview
  Scenario: targets output is unchanged
    When the characterization row "targets" runs
    Then it still matches

  @core
  Scenario Outline: destination resolution is a function of the environment
    Given an OpEnv with <flags>, cwd <cwd> and headless <headless>
    When core resolves the destination
    Then the result is <result>

    Examples:
      | flags                          | cwd                         | headless | result                                   |
      | --prefix dest                  | anywhere                    | false    | scratch tree at dest                     |
      | --labview-version 2026 --prefix x | anywhere                 | false    | error "mutually exclusive"               |
      | none                           | a project with a venv       | false    | the venv                                 |
      | none                           | a project without a venv    | false    | error naming lvpm venv create            |
      | none                           | a project without a venv    | true     | the LabVIEW the manifest's labview names |

  # Installing into a LabVIEW installation changes every project that uses it,
  # so lvpm does it only when --global is given; --labview-version only picks
  # which one. The venv still wins over --labview-version, --prefix needs no
  # --global, and a headless machine counts as consent.
  @core
  Scenario Outline: a write into LabVIEW itself needs --global
    Given an OpEnv with <flags>, cwd <cwd> and headless <headless>
    When core resolves the destination
    Then the result is <result>

    Examples:
      | flags                           | cwd                                      | headless | result                                       |
      | --labview-version 2025          | anywhere                                 | false    | error naming --global, with a next line      |
      | --labview-version 2026          | a project without a venv                 | false    | error naming lvpm venv create and --global   |
      | --global --labview-version 2026 | anywhere                                 | false    | LabVIEW 2026 itself                          |
      | --global --labview-version 2026 | a project with a venv                    | false    | LabVIEW 2026 itself                          |
      | --global --labview-version 1999 | anywhere                                 | false    | error naming the installed LabVIEWs          |
      | --global --prefix x             | anywhere                                 | false    | error "mutually exclusive"                   |
      | --global                        | anywhere                                 | false    | error "which LabVIEW?" naming both           |
      | --global                        | a project without a venv                 | false    | LabVIEW 2025 itself, no venv announced       |
      | --global                        | a project with a venv                    | false    | the venv's LabVIEW itself, no venv announced |
      | --global                        | a project whose labview is not installed | false    | error naming the manifest's labview          |
      | --labview-version 2025          | a project with a venv                    | false    | the venv                                     |
      | none                            | anywhere                                 | false    | error "pick a destination" naming --global   |
      | --labview-version 2026          | a project without a venv                 | true     | LabVIEW 2026, chosen headless                |
      | --labview-version 2026          | anywhere                                 | true     | LabVIEW 2026 itself                          |

  @core
  Scenario Outline: reading needs no --global
    Given an OpEnv with <flags>, cwd anywhere and headless false
    When core resolves the destination for reading
    Then the result is <result>

    Examples:
      | flags                  | result                                     |
      | --labview-version 2025 | LabVIEW 2025 itself                        |
      | none                   | error "pick a destination" naming --global |
