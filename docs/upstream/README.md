# Tracking Truce upstream

MOOSE tracks https://github.com/truce-audio/truce as a selective upstream,
not as an automatically merged branch. Local setup:

```sh
git remote add upstream https://github.com/truce-audio/truce.git
git fetch upstream main
```

<!-- coderabbit-ai-generated:v1 -->

<!-- coderabbit-ai-generated-section:v1:a26dea2fd9664f2f6c9052c9d1ced96548be30077e42978910ff2d8d1abbeb1e -->

The **Truce upstream review** workflow runs weekly on Mondays at 06:00 UTC
and on manual dispatch. It paginates all open and closed issues and PRs and
checks main's commit. The report is written to the job log and job summary.
Pending changes since `truce.json` do not fail the run or trigger a
failure notification. The workflow never posts comments, executes upstream
code, merges changes, or rewrites the baseline automatically.

After reviewing the report and updating the disposition below:

```sh
GH_TOKEN=$(gh auth token) python3 scripts/truce-upstream.py --accept
git add docs/upstream/truce.json docs/upstream/README.md
```

Commit the reviewed baseline. Normal build CI is independent of this report.
To see the report locally, omit `--accept`. A successful report exits with
status 0 even when changes are pending. Network and API errors still fail
the command without recording a partial snapshot.


<!-- /coderabbit-ai-generated:v1 -->
## Reviewed 2026-09-27

33 issues and 194 PRs (all states) were inventoried; historical release rollups
were triaged as inherited/reference work rather than blanket cherry-picks.

| Upstream | Disposition |
|---|---|
| [#224](https://github.com/truce-audio/truce/pull/224) | Ported same-name archive member preservation, with BSD/GNU-name and exact-duplicate regression coverage. VST3 IID half already present. |
| [#223](https://github.com/truce-audio/truce/issues/223) | Fixed audio input policy in driver and standalone; use buses instead of the Effect category. |
| [#235](https://github.com/truce-audio/truce/pull/235), [#236](https://github.com/truce-audio/truce/pull/236) | Already present: CLAP recall rescan and VST3 interface IDs. |
| [#178](https://github.com/truce-audio/truce/pull/178), [#126](https://github.com/truce-audio/truce/pull/126), [#234](https://github.com/truce-audio/truce/issues/234) | Already handled: remote controls, MUI hidden-window rendering, macOS content-view attachment order. |
| [#122](https://github.com/truce-audio/truce/pull/122) | Do not port: upstream reverted after REAPER regressions. |
| [#177](https://github.com/truce-audio/truce/pull/177) | Do not port: incompatible percent-unit convention. |

## Follow-ups (not part of this fix batch)

- [#230](https://github.com/truce-audio/truce/issues/230): constants/expressions
  in smoothing and range syntax. Constant defaults already work. Include
  compile-pass/fail examples; keep existing literal syntax compatible.
- [#229](https://github.com/truce-audio/truce/issues/229): fixed arrays of nested
  parameters. Specify stable IDs and append/reorder rules; test saved automation.
- [#231](https://github.com/truce-audio/truce/pull/231): add a host-dirty API for
  persisted non-parameter state across CLAP/VST3/AU/standalone. Dirty notification
  and host undo grouping are separate requirements. Do not import the optional
  sample loader merely to gain its design rationale.
- [#233](https://github.com/truce-audio/truce/issues/233): implement or clarify
  Editor::idle scheduling. MUI has its own frame loop; avoid duplicate polling.
- CLAP latency: move changed notifications into activation; request restart
  while active. Previously reproduced by Korrekt's param-set-events validation;
  deferred separately, not silently included in this upstream batch.
- [#222](https://github.com/truce-audio/truce/pull/222) and #231 loader: optional
  DSP/file utilities only when a consuming product needs them.

The source of the archive port is Truce PR #224, head
`795a656dee875b90ae52bfcc515a757da6e0da24`; attribution is retained in the commit.
