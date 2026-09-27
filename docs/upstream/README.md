# Tracking Truce upstream

MOOSE tracks https://github.com/truce-audio/truce as a selective upstream,
not as an automatically merged branch. Local setup:

```sh
git remote add upstream https://github.com/truce-audio/truce.git
git fetch upstream main
```

The **Truce upstream review** workflow runs daily and on manual dispatch. It
paginates all open/closed issues and PRs and checks main's commit. Changes since
`truce.json` produce a review summary and a failed scheduled run, so workflow
subscribers can receive GitHub notifications. It never posts comments, executes
upstream code, merges changes or rewrites the baseline automatically.

After reviewing the report and updating the disposition below:

```sh
GH_TOKEN=$(gh auth token) python3 scripts/truce-upstream.py --accept
git add docs/upstream/truce.json docs/upstream/README.md
```

Commit the reviewed baseline. Normal build CI is independent of this review gate.
To see a report locally, omit `--accept`; exit 1 means review is needed, while
network/API errors raise an error rather than recording a partial snapshot.

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
