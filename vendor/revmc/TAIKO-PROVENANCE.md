# revmc provenance

This directory vendors the revmc source at commit
`4042c2ed50d88fb16505976461f8be1d13398f4a` from
<https://github.com/paradigmxyz/revmc>.

The copy contains only the crates required by Alethia Reth. It retains revmc's
original MIT and Apache-2.0 licenses.

Alethia Reth backports the compatible portions of these upstream changes:

- <https://github.com/paradigmxyz/revmc/pull/391> for started-worker tracking
  and non-blocking runtime controls;
- <https://github.com/paradigmxyz/revmc/pull/395> for dynamic-gas failure
  ordering and configurable exact error reporting.

The local runtime control API additionally makes cache clearing non-blocking and
returns explicit errors for unavailable or saturated command channels.
