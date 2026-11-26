Feature: VI properties over VI Server

  Issue #78. VIAttrVector, the request that reads and writes a VI's properties,
  as captured from LabVIEW 2026 Q3's own VI Server client on 2026-11-26.

  Scenario: the protocol document gives the property layout
    When the VI Server protocol document is read
    Then its "Properties" section shows the request and the reply field by field
    And it says a read declares the type it wants back
    And it says an enum property is declared as an enum, and a plain U32 is error 42
    And it says the ids are the codes LabVIEW stores in Property and Invoke Nodes

  # Manual evidence: vi-probe needs a LabVIEW that answers VI Server. Run by
  # hand from a project whose venv LabVIEW is up.
  @labview @unsafe
  Scenario: vi-probe reports a VI that runs as Idle
    When I run vi-probe on the check fixture "Good.vi"
    Then stdout says "state Idle"

  @labview @unsafe
  Scenario: vi-probe reports a VI whose subVI is gone as Bad
    When I run vi-probe on the check fixture "Broken.vi"
    Then stdout says "state Bad"
