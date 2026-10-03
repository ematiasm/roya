# Feature: CodeQL quality gate

## Objective

Configure GitHub CodeQL code scanning for the Rust application and require its blocking severity results on pull requests once the remote repository rule is enabled.

## Problem

The repository already has a GitHub Actions checks workflow, but no CodeQL configuration. Enabling the repository's code-quality merge requirement before a CodeQL analyzer exists could block pull requests without a result to evaluate.

## Why

- Roya handles authentication, authorization, SQL, migrations, money, and stock state.
- CodeQL provides an independent static-analysis signal for the Rust code.
- The merge rule should be enabled only after CodeQL is configured and producing results.

## Scope

- Add `.github/workflows/codeql.yml` using the official CodeQL actions.
- Analyze Rust on pull requests targeting `main`, pushes to `main`, and a weekly schedule.
- Use CodeQL's no-build mode for Rust, which analyzes the source without coupling analysis to a second production build command.
- Request the minimum permissions needed for checkout and security-event upload.
- Enable the repository's code-quality merge protection at the remote GitHub repository after explicit authorization for the authenticated GitHub session.

## Constraints

- Do not change application behavior or unrelated source files.
- Do not enable the remote merge rule before the workflow is committed and available to the repository; if the user declines remote mutation, leave the rule unchanged and report it.
- Use English for the technical artifact.
- Verify YAML structure and repository diff locally; a GitHub-hosted workflow execution requires the change to reach the remote repository.

## TDD and checks

- TDD is not applicable to a static workflow configuration; the applicable checks are YAML/workflow validation, `git diff --check`, and repository status review.
- Runtime harness: N/A; this change adds CI configuration only and does not alter application runtime behavior.
- Rollback boundary: remove `.github/workflows/codeql.yml` and, if separately applied, remove the remote code-quality branch rule.

## Work units

- [x] T1 — Add and validate the CodeQL workflow.
      Evidence: `.github/workflows/codeql.yml` added with Rust no-build analysis, pull-request/push/weekly triggers, and least-privilege permissions; `git diff --no-index --check` passed. `actionlint` and a local YAML parser are unavailable, so GitHub-hosted execution remains pending.
- [x] T2 — Enable the remote code-quality merge rule with the authorized GitHub session.
      Closed 2026-10-02 **against the tree**: parent-verified via `gh api repos/ematiasm/roya/rulesets/23498167` — ruleset "main", `enforcement: active`, updated 2026-09-24, with a `code_quality` rule at `severity: errors` alongside `deletion`, `non_fast_forward` and `pull_request`.
- [x] T3 — Record final evidence and hand off the result.
      Evidence: PR #102 opened; GitHub-hosted CodeQL run `36063657254` passed on commit `ee4e18f`. The Code Quality setup endpoint reported the feature unavailable for this repository at that time — it is active on `main` as of 2026-09-24, per T2 above.

## Acceptance criteria

1. The workflow is valid GitHub Actions YAML and targets Rust.
2. CodeQL runs on pull requests to `main`, pushes to `main`, and a weekly schedule.
3. The workflow grants only the permissions needed for CodeQL and checkout.
4. No unrelated files are modified.
5. The remote merge rule is enabled only if the user authorizes the GitHub credential/session, and its actual outcome is reported.

## Progress and evidence

- Feature document created: `odd/tasks/codeql-quality-gate.md`.
- Commit: `d90b6d1` created locally; direct push to `main` was rejected by the active pull-request ruleset, and the commit is now published on `ci/codeql-quality-gate` for a pull request.
- T1: completed; workflow added and local diff validation passed.
- T2: closed 2026-10-02; the code-quality rule is active on `main` at `severity: errors` (ruleset updated 2026-09-24). The authorized GitHub API reported Code Quality unavailable for this repository when it was first attempted, which is why this was recorded as blocked at the time.
- T3: completed; PR #102 is open and the GitHub-hosted CodeQL run passed. Code Quality was unavailable at that time, so its merge rule was not enabled then; it is active on `main` as of 2026-09-24.
- T3 evidence: CodeQL run `36063657254` passed on commit `ee4e18f`; PR: https://github.com/ematiasm/roya/pull/102

## Next step

Merge PR #102 under the repository's normal review policy. The code-quality merge rule is active on `main` (severity errors, as of 2026-09-24); nothing is pending on it.
