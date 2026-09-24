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
- [ ] T2 — Enable the remote code-quality merge rule with the authorized GitHub session.
- [ ] T3 — Record final evidence and hand off the result.

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
- T2: blocked; the authorized GitHub API reports that Code Quality is not available for this repository, so `Require code quality results` cannot be enabled. The separate CodeQL workflow is ready for pull-request validation.
- T3: pending; open the pull request and verify the GitHub-hosted workflow after publication.

## Next step

Open the pull request for `ci/codeql-quality-gate`, wait for the GitHub-hosted CodeQL workflow, and report that Code Quality itself remains unavailable for this repository.
