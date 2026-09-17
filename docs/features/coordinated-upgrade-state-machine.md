# Coordinated upgrade state transitions

This is the contract being used to complete the coordinated offline upgrade,
not a claim that the current implementation satisfies every row. Existing code
and tests are retained. The [full-store migration brief](full-store-migration.md)
owns the operator workflow, supported profiles and evidence limitations.

The journal records intent and completed transitions. It does not by itself
prove the state of the files. A process can stop between an effect and its
completion record. Tests must therefore describe both journal and filesystem
state; expected results must not come from the production phase classifier.

Describe a fixture with the tuple: last intact journal record and journal temp;
live main/WAL/journal; retained components; all publication temporary names;
preserved artifact identities; and running upgrader identity. A main file can
match both original and candidate bytes: those are predicates, not mutually
exclusive state labels. Record missing, empty, known and unknown components
separately. `status` does not require the mutating command's executable gate.

Every component observation distinguishes an absent directory entry from a
dangling link. Main, WAL, SHM and rollback-journal entries at both live and
retained locations must be admitted without following leaf aliases; an occupied
component must be a regular file. This applies during preparation and recovery,
not only to publication temporaries. A valid live copy never excuses a foreign
occupied retained copy. SHM's untracked content does not exempt its file type.
Identity-bearing artifacts and published journal records must likewise be
regular, non-aliased files; the journal container must be a non-aliased
directory. A link to the expected bytes is not the expected owned file entry.

## Preconditions and invariants

- **Offline interval:** the operator keeps every other consumer stopped from
  source capture through finalization or completed rollback. Confirmation is
  an assertion, not an admission lock. No host integration is required.
- **Complete preflight:** validate the entire relevant file and journal plan
  before the first move, unlink, publication or journal append. A contradiction
  already present at entry must refuse without changing the relevant inventory.
  An I/O failure after valid effects is instead an interrupted transition.
- **Preserved original:** before removing a live candidate, verify that every
  durable original component is available in its live or retained location.
  Equal candidate/original bytes do not identify which transition occurred.
- **Accounted cleanup:** a reserved filename alone does not establish ownership.
  Validate phase, file type, bytes and relevant identities together. An owned
  temporary file must not be removed before a contradiction elsewhere is found.
- **Intent before database effects:** only fully accounted temporary cleanup may
  precede an intent record. Retaining or publishing database components follows
  `Activating`; removing a verified live candidate or restoring originals follows
  `RollingBack`. Every later journal record preserves the prepared identities,
  changing only sequence and kind.
- **No unknown-write bypass:** neither a matching main file nor a journal phase
  excuses changed or unexpected durable sidecars. Do not open the activated
  target with ordinary store APIs before finalization.
- **Terminal decision:** finalization closes automatic rollback permanently;
  completed rollback cannot reactivate this operation. Later permitted consumer
  writes do not turn a terminal operation back into an incomplete one.
- **Honest limits:** incomplete initial preparation and corrupt published
  journals require preservation and diagnosis. Process-exit tests do not prove
  power-loss durability. Required operation artifacts remain retained.

Here, a *complete original* means the prepared main file and the recorded
presence and bytes of WAL and rollback-journal components. Absent and empty
files are different states. SHM is coordination state, not a substitute for WAL
data; tests must state its separate preservation/cleanup expectation.

## Transition table

Rows are semantic cases, not new persisted phase values. Every mutating row
requires the common preflight above, maintained downtime and the recorded
upgrader identity. `status` observes; it must not repair a row into validity.

Successful nonterminal `status` means that the files agree with the journal and
the phase's continuation plan is ready. It performs the same read-only component
and reserved-temporary ownership checks, but neither requires downtime or the
recorded executable nor performs cleanup, moves or journal appends. A valid
temporary awaiting cleanup stays present. A foreign temporary, missing original
or contradictory component refuses instead of reporting a clean phase.
Terminal status retains T8 semantics: later legitimate live writes are not
reclassified, required artifacts remain checked, and `Finalized` still requires
the retained original.

Guard order `G` means: path admission and intact legal journal → preserved
artifact identities → same upgrader for mutations → legal requested action →
complete live/retained component plan → all reserved temporary decisions.
These are read-only checks. Temporary reconciliation executes only after the
whole plan passes. Names such as `retain_plan_ready`, `restore_plan_ready` and
`retained_identity_ready` identify current implementation responsibilities, not
an exemption for a caller that takes a different branch.

| Case | Guards in order after common admission | Next stable journal state |
| --- | --- | --- |
| T1 | Admit source/operation/reserved sidecars and executables → capture original identities → copy/checkpoint detached backup → validate export/import → recheck unchanged source → publish initial record | `Prepared` |
| T2 | G with complete `retain_plan_ready`, including every occupied retained destination, before recording intent | `Activated` |
| T3 | G with per-component retention and all staging ownership checked before any remaining move; `recover` with untouched original performs no database move | `Activated`, or unchanged `Activating` for untouched-original recovery |
| T4 | G with complete retained original and candidate live, regardless of whether `Activated` was already recorded | `Activated` |
| T5 | G with `restore_plan_ready`; deletion requires candidate main bytes, verified retained original main and reconciled durable sidecars; never delete an already restored original | `RolledBack` |
| T6 | G with per-component restoration; preserve already restored components and classify equal-content main by locations and recorded intent | `RolledBack` |
| T7 | G with complete retained original, candidate live and combined temporary validation before any cleanup or record append | `Finalized` |
| T8 | Intact terminal journal → required artifacts and applicable retained material → owned-temp cleanup plan for `recover`; no live-content reclassification after permitted resume | Same terminal state |
| T9 | G; last record is `Prepared`, original untouched | `Prepared` (no append) |
| T10 | G; last record is `Activating`, original untouched, all reserved destinations accounted | `RolledBack` |

| Case | Journal and observed files | Operation | Required effects and result | Interruption/retry |
| --- | --- | --- | --- | --- |
| T1 | No operation; admitted source and paths | `prepare` | Capture coherent source including present empty sidecars; validate archive and candidate; recheck source identity; publish `Prepared` | Before initial record publication, preserve incomplete artifacts and refuse to invent preparation |
| T2 | `Prepared`; original fully live; no contradictory reserved destinations | `activate` | Record `Activating` before retention; retain original components; publish candidate without replacing unknown files; record `Activated` | T3 or T4 accounts for each completed effect |
| T3 | `Activating`; original components wholly live, split, or wholly retained; no candidate publication established by locations | `activate` / `recover` | Validate every component and temporary. `activate` finishes retention/publication. `recover` does so after retention started; with original wholly live and no retention, it reports `Prepared` without moving the database or erasing the intact `Activating` intent | Untouched-original recovery remains T3; later `activate` continues, or `rollback` follows T10 |
| T4 | `Activating` or `Activated`; candidate live; complete retained original | `activate` / `recover` | Validate all reserved state, reconcile owned publication temporaries, append only a missing legal `Activated` record | No duplicate completion and no loss of rollback material |
| T5 | `Activating` or `Activated`; original recoverable component by component | `rollback` | Record `RollingBack`; remove only verified candidate material; restore original; remove accounted publication temporaries; verify and record `RolledBack` | T6 handles partial restoration, including identical main-file bytes |
| T6 | `RollingBack`; original partly or fully restored | `rollback` / `recover` | Restore only missing verified components; preserve already restored ones; finish cleanup and record `RolledBack` | `activate` and `finalize` refuse without effects |
| T7 | Candidate live and complete retained original; `Activated` or missing only its completion record | `finalize` | Validate all reserved state before cleanup; record missing legal `Activated` if needed, then `Finalized` | An intact final decision remains terminal even if cleanup/reporting was interrupted |
| T8 | Intact terminal `Finalized` or `RolledBack` journal and required artifacts | `status` / `recover` | Report the terminal decision; any permitted owned-temp cleanup is fully preflighted and cannot change that decision | Never infer an earlier phase from later legitimate live-store writes |
| T9 | `Prepared` only; unchanged original still live | `rollback` | No activation to undo; leave source and prepared phase unchanged | This is not a completed rollback transition |
| T10 | `Activating` intent only; complete original still live; no retention effects | `rollback` | After complete preflight, record `RollingBack` then `RolledBack`, without replacing the original | Distinguish this from the no-op in T9 |

Published journal edges are: initial → `Prepared` → `Activating` → `Activated`
→ `Finalized`, with `Activating` or `Activated` → `RollingBack` → `RolledBack`.
Retry and read operations need not append an edge. A complete live publication
with a missing `Activated` record is not permission to skip that journal edge.
The reported `Prepared` phase after untouched-original recovery describes file
progress, not removal of the existing activation intent. Equal original and
candidate bytes alone cannot establish publication or require a retained copy
that has not yet been created.

Journal/file contradictions are checked before cleanup as well as before the
next append. `Prepared` cannot coexist with retention or publication effects:
the `Activating` intent must precede them. An intact `Activated` record requires
the candidate live and complete retained original; a missing live main is not
an activation interruption in that state. In contrast, `Activating` may have
only SHM, WAL or rollback journal already retained and an equal-content original
main still live. That is T3, not publication: retained-main location evidence
is still missing. Empty durable sidecars are included in this rule.

For `Activating`, publication requires both candidate bytes at the live main
and original bytes at the retained main. Without that location evidence,
continuation validates the retention plan, not the restoration plan. The main
is retained after all sidecars: a missing live main therefore requires every
live WAL, journal and SHM entry to be absent before cleanup or further moves.
This includes an unexpected live-only SHM, not just duplicate live/retained SHM.
A distinct candidate live with neither live nor retained original is an
impossible publication state; `status` and mutations refuse it without effects.

Rollback entering from `Activating` must first validate this retention-state
plan, then its restoration plan and all temporary decisions, before appending
`RollingBack` or changing any file. Under an already recorded `RollingBack`,
restored sidecars may legitimately be live before the main is restored; use the
restoration plan there, not the missing-live-main retention rule. The same SHM
placement can therefore be contradictory under `Activating` and valid under
`RollingBack`: intent explains which effect order could have produced it, but
does not replace independent component checks. Rollback status must validate
those components too, including occupied corrupt retained twins and aliases.

## Contradiction and boundary families

Use representative equivalence classes, not the full Cartesian product. Keep
combined cases where one valid cleanup opportunity can hide another bad file.

| Family | Required expectation | Main rows |
| --- | --- | --- |
| C1: occupied or corrupt retained destination, missing retained component | Refuse before journal append, sidecar retention, cleanup or successful recovery; test with intact `Activated` record as well as missing completion record | T2–T7 |
| C2: journal temp, partial copy and complete staging coexist, one is foreign | Validate all three before changing any; include live-linked, activated, and untouched Prepared/intent-only recovery branches | T3–T7, T9–T10 |
| C3: absent, zero-byte, prefix, complete and foreign file bytes | Preserve absence versus empty sidecars; allow legitimate empty copies; reuse complete partial copies for activation but clean them on completed rollback | T1, T3–T6 |
| C4: process exit during copy or record write, after sync, link, move or unlink | Account for both sides of each effect; include a second interruption during recovery, not only one uninterrupted retry | T1–T8 |
| C5: original and candidate main bytes are equal | Decide retention and restoration from the complete component/location state, never candidate hash equality alone; include interruption before the first retention effect | T2–T6 |
| C6: reserved aliases, descendants, symlinks and filesystem case rules | Refuse unsafe or ambiguous source paths before creating operation files; admit live/retained components, identity artifacts and journal entries without following aliases before recovery effects. Keep explicit platform limits | T1–T8 |
| C7: extra or changed live WAL/journal, corrupt candidate or journal | Preserve evidence and refuse; neither cleanup nor a later phase label blesses unknown bytes | T2–T8 |
| C8: operation volume and activated-file permissions | State same-volume and hard-link prerequisites and resulting owner/mode/ACL; do not promise preservation of the original access descriptor | T1–T2, T7 |
| C9: journal kind contradicts file locations or identities | `Prepared` cannot have retention/publication effects; `Activated` requires the candidate live, not a missing main or a different original, even with intact retained material. Refuse without effects, including `status`; contrast reachable `Activating` intent before retention | T2–T7, T10 |

## Test and completion discipline

Each case names its row/family, explicit expected result, allowed effects and
recovery endpoint. Refusal tests compare a no-follow inventory before and after,
including journal and temporary files, not only main database bytes. Successful
recovery checks final content, sidecar presence, temporary cleanup and journal
sequence separately. An oracle must not call the production classifier to decide
whether its own result is correct.

Reuse existing tests first. Maintain a bounded mapping from fault hooks and
existing test names to these semantic rows; enum hooks are an inventory of
instrumented boundaries, not proof that every possible interruption is covered.
Include missing mid-effect windows explicitly. Prefer exhaustive test-side hook
mapping so adding an instrumented boundary requires identifying its case.
Do not build a second general phase classifier as the expected-result oracle:
use explicit fixture predicates, byte equality, exact journal sequences and
allowed-effect assertions, then compare the public status separately.

Author missing baseline tests before the related
production correction, and record failures with their product/test/environment
classification. Iterate with the smallest relevant tests. Once the mapped cases
are satisfied, run required project gates and the independent review pair on one
coherent candidate. Record unexecuted platform and fault boundaries explicitly;
the existence of a test or a table row is not execution evidence.

## Finding and regression traceability

The table below groups existing tests by the contract they exercise; it is not
a claim that their coverage closes every case in that family. Names refer to
the [unit suite](../../src/storage/migration/upgrade/tests.rs) and
[CLI suite](../../tests/upgrade_cli.rs). Added regression names should preserve
the row/family in a nearby comment. Execution evidence belongs to the work
record, not this document.

| Finding or boundary | Contract | Regression anchor | Discriminator / coverage boundary |
| --- | --- | --- | --- |
| Source changes during preparation; detached sidecar checkpointing; backup sync handle | T1, C3/C7 | `upgrade_prepare_captures_source_identities_before_backup`, `upgrade_prepare_refuses_when_source_changes_before_prepared`, `upgrade_cli_empty_wal_roundtrip_preserves_absence_and_empty` | Valid zero-byte WAL from a child exit, not merely missing WAL; populated WAL has separate CLI coverage |
| Reserved database/sidecar path aliases and no-follow inventories | T1, C6 | `upgrade_cli_reserved_sidecar_operation_paths_refuse_before_effects`, `upgrade_cli_ambiguous_unicode_sidecar_paths_refuse_before_effects`, `upgrade_cli_unicode_case_sidecar_aliases_refuse_before_effects` | All-platform conservative admission plus a real Windows alias fixture; this does not execute every filesystem's case/normalization rules |
| Original split by activation, then interrupted rollback | T3/T5/T6, C4 | `upgrade_split_activation_then_interrupted_rollback_recovers`, `upgrade_cli_double_interruption_recovers_split_original_components` | Split-retain recovery with a pre-existing foreign staging file |
| Equal original/candidate main bytes | T2–T6, C5 | `upgrade_equal_content_from_current_import_fixed_point`, `upgrade_c5_activating_intent_before_retain_equal_content_keeps_original` | Independent location/component assertions before first retention and after interrupted restoration |
| Wrong retained destinations before first activation | T2, C1 | `upgrade_wrong_retained_refuses_before_reserved_temp_cleanup` | `Prepared` with live WAL and wrong retained main: no journal or file effects |
| Restored original plus corrupt retained twin | T6, C1 | `upgrade_review_v5_t6_c1_restored_wal_corrupt_retained_refuses_before_effects`, `upgrade_review_v5_t6_c1_restored_main_corrupt_retained_refuses_before_effects` | A valid live component cannot hide a foreign occupied retained component |
| Equal-content main after SHM retention | T3, C5 | `upgrade_review_v5_t3_c5_retained_shm_equal_main_completes_by_location` | Each fixture asserts equal bytes and absent retained main before continuing; final live and retained locations are checked independently |
| Equal-content main after empty durable-sidecar retention | T3, C3/C5 | `upgrade_review_v5_t3_c5_retained_empty_wal_activate_completes_by_location`, `upgrade_review_v5_t3_c5_retained_empty_wal_recover_completes_by_location`, `upgrade_review_v5_t3_c5_retained_empty_journal_activate_completes_by_location`, `upgrade_review_v5_t3_c5_retained_empty_journal_recover_completes_by_location` | Separate fixtures reach each action; equal main bytes, absent retained main and retained empty sidecar are asserted before continuation |
| Unexpected live SHM after main retention, with an owned journal temp | T3, C1/C2 | `upgrade_review_v5_c1_c2_retained_main_dual_shm_legal_temp_activate_refuses_before_effects`, `upgrade_review_v5_c1_c2_retained_main_live_only_shm_legal_temp_recover_refuses_before_effects` | Both activate and recover have separate dual-SHM and live-only-SHM tests; refusal preserves the complete no-follow inventory and journal temporary |
| Missing recoverable original, with candidate live or both main locations absent | C7/C9 | `upgrade_cli_review_v6_candidate_without_original_refuses_without_effects`, `upgrade_cli_review_v6_retained_shm_does_not_replace_missing_original`, `upgrade_cli_review_v6_both_main_locations_missing_refuses_without_effects`, `upgrade_cli_review_v6_retained_shm_without_either_main_refuses_without_effects` | Independent original/candidate byte inequality, with and without retained coordination state; status and every mutating action must refuse with unchanged no-follow inventory |
| Nonterminal rollback diagnosis validates components and reserved ownership | T6, C1/C2/C6/C9 | `upgrade_cli_review_v7_rolling_back_candidate_without_original_status_refuses`, `upgrade_cli_review_v7_rolling_back_both_mains_missing_status_refuses`, `upgrade_cli_review_v7_rolling_back_corrupt_retained_twin_status_refuses`, `upgrade_cli_review_v7_foreign_staging_status_refuses`, `upgrade_cli_review_v7_foreign_partial_status_refuses`, `upgrade_cli_review_v7_foreign_journal_temp_status_refuses` | Independent status fixtures preserve whole no-follow inventory; `upgrade_cli_review_v7_rolling_back_valid_status_preserves_inventory` is a legal positive control |
| Retention-state SHM rules apply before entering rollback | T3/T5/T6, C1/C2 | `upgrade_review_v7_rollback_activating_missing_main_dual_shm_refuses_before_effects`, `upgrade_review_v7_rollback_activating_missing_main_live_only_shm_refuses_before_effects`, `upgrade_review_v7_status_rolling_back_restored_wal_before_main_reports_rolling_back` | Invalid Activating placement refuses before effects; already RollingBack with a legitimately restored sidecar remains valid |
| Equal-content split-retention rollback preserves the only live original | T3/T5, C3/C5 | `upgrade_review_v7_t3_c5_retained_empty_wal_rollback_completes_by_location`, `upgrade_review_v7_t3_c5_retained_empty_journal_rollback_completes_by_location` | Retained main absent before rollback; original bytes remain live, empty sidecar is restored, exact journal sequence and absent temporary entries are checked |
| Reserved-temp diagnosis across the other nonterminal phases | T2/T3/T4/T9/T10, C2 | `upgrade_review_v7_status_nonterminal_foreign_temps_refuse_before_effects` | Explicit Prepared, intent-only Prepared, actual Activating split-retain and Activated fixtures crossed with three foreign temporary kinds; collect all twelve independent results before asserting |
| No-follow admission across callers | T1/T6, C6 | `upgrade_review_v5_c6_prepare_shm_directory_refuses_before_effects`, `upgrade_review_v5_c6_rolling_back_absent_dangling_retained_wal_refuses_before_effects`, `upgrade_review_v5_c6_candidate_symlink_equal_bytes_refuses_before_effects`, `upgrade_review_v5_c6_journal_record_symlink_equal_bytes_refuses_before_effects` | Untracked or absent content expectations do not waive entry-type checks; equal artifact or journal bytes behind an alias do not establish ownership |
| Retained source missing after activation | T4/T7, C1 | `upgrade_c1_activated_corrupt_retained_refuses_before_effects`, `upgrade_cli_model_activated_missing_retained_refuses_status_and_recover` | Intact `Activated` record with corrupt/absent retained main; shared component validation also covers durable sidecars, without claiming every component mutation was independently executed |
| Partial publication and owned staging cleanup | T3–T6, C2/C3/C4 | `upgrade_mid_copy_child_exit_recovers_activate_and_rollback`, `upgrade_staging_hard_link_child_exit_drops_alias`, `upgrade_non_candidate_partial_is_preserved` | Full copy synced but not linked, then rollback: no completed partial left behind |
| Journal publication and combined preflight ordering | T3/T4/T7, C2/C4 | `upgrade_reserved_temps_contradiction_preserves_both`, `upgrade_journal_temp_survives_corrupted_candidate_refusal`, `upgrade_cli_journal_partial_recovers_only_expected_record_bytes` | Valid journal temp plus foreign staging/partial at live-linked and `Activated` boundaries, and inverse contradiction |
| Missing activated completion record; deterministic finalize | T7 | `upgrade_finalize_after_live_publication_records_legal_journal`, `upgrade_cli_finalize_never_reports_success_with_an_illegal_journal_transition` | Keep exact legal journal sequence and one expected result, not success-or-refusal |
| Rolling-back exclusivity and terminal closure | T6/T8 | `upgrade_rolling_back_status_refuses_activate_and_finalize_before_effects`, `upgrade_rolled_back_stays_terminal_after_live_resume`, `upgrade_cli_damaged_finalize_record_never_reopens_rollback` | Preserve terminal artifact requirements without interpreting new legitimate live bytes as an incomplete operation |
| Prepared no-op versus intent-only rollback | T9/T10, C2 | `upgrade_rollback_from_activating_intent`, `upgrade_cli_prepared_rollback_distinguishes_intent`, `upgrade_cli_prepared_combined_temps_refuse_before_cleanup` | Pair exact legal positive results with a legal journal prefix plus foreign staging; no branch skips common preflight |
| Impossible hand-restored state | C9 | `upgrade_cli_model_hand_restored_activated_state_is_not_prepared`, `upgrade_c9_activated_original_live_without_retained_refuses_before_effects` | Legal `Activated` journal plus original live and no retained source refuses; contrast reachable intent-only `Activating` |
| Journal/file contradiction before cleanup | C9, T2–T7 | `upgrade_cli_review_v5_activated_missing_live_refuses_before_effects`, `upgrade_cli_review_v5_prepared_retained_missing_live_refuses_before_effects`, `upgrade_cli_review_v5_prepared_retained_candidate_live_refuses_before_effects` | Start from real activation; remove live main or intent/completion records while retaining a candidate staging copy. All actions must refuse with unchanged no-follow inventory |
| Different original copied live while retained original remains | C9, T4/T7 | `upgrade_cli_review_v5_activated_original_live_with_retained_refuses_before_effects` | Independently assert original and candidate differ; a matching retained copy does not excuse the wrong live identity |
| Same-volume and staged-file access requirements | C8 | Operator disclosure in the migration brief | No new permission-preservation promise; record actual exercised platforms |

Corrupt published journal shapes (torn last, malformed earlier record, sequence
gap or changed prepared identity), missing/tampered artifacts, unknown live
bytes, and missing both live and retained originals are refusal families, not
additional recovery transitions. Instrumented fault boundaries must map to a
transition row; unsupported operation relocation, executable replacement,
concurrent writers, missing terminal artifacts and power-loss durability stay
explicit limitations rather than inferred successful recovery.
