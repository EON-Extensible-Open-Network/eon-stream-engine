# EON Module ABI Exception

**Version 1.0 — 2026-09-25**
**Status: DRAFT.** This text has not yet been reviewed by a lawyer. See madde 13 of
the project plan (`eon-docs/plan/eon-plan.md`). Do not rely on it as legal advice.

---

## Additional permission under GNU GPL version 3 section 7

The copyright holders of this Program grant you the following additional permission,
as provided by section 7 of the GNU General Public License version 3.

You have permission to combine this Program with **Independent Modules**, and to convey
the resulting combined work, distributing the Independent Modules under terms of your
own choosing — including terms that are not compatible with the GNU GPL — provided that
all of the following conditions are met:

1. **Defined boundary.** The Independent Module communicates with this Program
   *exclusively* through the **EON Module ABI**, as published in the `eon-stream-spec`
   repository (`docs/module-abi.md`) at a released version. Communication through any
   other interface — direct linking against this Program's internal symbols, inclusion
   of its source, use of its private data structures, or patching its build — is
   outside this permission.

2. **No derivation.** The Independent Module does not contain, copy, or derive from
   source code of this Program, other than the interface declarations published in
   `eon-stream-spec`, which are licensed separately under Apache-2.0.

3. **GPL still applies to this Program.** You continue to comply with the GNU GPL in
   full for this Program itself, including the obligation to convey its Corresponding
   Source when you convey the combined work.

4. **No implied grant over other components.** This permission covers only the
   combination described above. It grants no rights in this Program beyond that, and
   grants nothing at all in third-party components this Program depends on (see
   `NOTICE`).

If you modify this Program, you may extend this exception to your modified version,
but you are not obliged to. If you do not wish to extend it, delete this statement
from your version.

---

## Why this exception exists

The project is copyleft on purpose: the core must stay open, and no vendor should be
able to take it into a closed product (madde 33 of the plan).

But the module system is a deliberate extension point (madde 3, 4). Institutions will
eventually need integrations the project itself cannot ship — a closed e-Okul connector
is the concrete case (madde 20). Without this exception, a strict reading of the GPL
would make any in-process module a derivative work and block that outright.

It is written **now, at the first commit**, and not later, for a practical reason:
contributions are accepted under the DCO (`CONTRIBUTING.md`), so every contributor keeps
their own copyright. Adding an exception afterwards would require the agreement of every
contributor who ever touched the code — in practice, impossible. Granting it on day one
costs nothing; retrofitting it costs the project.

## Scope

This exception applies to the repositories that third-party modules link against:

- `eon-stream-core`
- `eon-stream-engine`
- `eon-stream-app`

It does **not** apply to `eon-edu-server` (AGPL-3.0-or-later, no exception): the
institution server is a network service, which is exactly where the copyleft is meant
to have teeth.

## Contributing under this exception

By submitting a contribution to a repository that carries this file, you license it
under **GPL-3.0-or-later WITH the EON Module ABI Exception 1.0**. If you are not
willing to grant the exception, say so in the pull request and it will not be merged
into these repositories — this is not a judgement of the contribution, only of where
it can live.

## SPDX

There is no registered SPDX identifier for this exception. Source files use:

```
// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).
```
