# Contributing to uptimepage

Thank you for considering a contribution. This document covers 
what you need to know before opening a pull request.

## License agreement

By contributing to uptimepage, you agree that your 
contributions will be licensed under **AGPL-3.0**, the same 
license as the rest of the project.

We use the [Developer Certificate of Origin (DCO)](https://developercertificate.org/) 
to ensure contributors have the right to submit their work. Every 
commit must include a `Signed-off-by` line:

    Signed-off-by: Your Name <your.email@example.com>

You can add this automatically by committing with `git commit -s`. 
Configure your git identity once:

    git config --global user.name "Your Name"
    git config --global user.email "your.email@example.com"

By signing your commits, you certify the terms at 
https://developercertificate.org/ — essentially that you wrote 
the code yourself or have the right to contribute it.

PRs without DCO sign-off cannot be merged.

## Code of conduct

Be kind. Be specific. Assume good faith.

## Before you open a PR

1. **Open an issue first** for non-trivial changes. Describe what 
   you want to do and why. This avoids you doing work that we 
   can't accept.

2. **Run the tests:**

       cargo test
       cargo clippy --all-targets -- -D warnings
       cargo fmt --check

   All three must pass.

3. **Add tests** for new functionality. Bug fixes should include 
   a regression test.

4. **Update documentation** when behavior changes. The docs live 
   under `docs/`; the website publishes them at 
   <https://uptimepage.dev/docs>.

## What we're looking for

Good first contributions:
- Bug fixes with clear reproduction steps
- Documentation improvements
- New target check types (e.g., ICMP, DNS-only, gRPC)
- New notifier integrations
- Performance improvements with benchmark evidence

Please discuss before working on:
- Architectural changes
- New API endpoints not in the existing OpenAPI spec
- Database schema changes
- License changes (the answer will almost certainly be no)

## What we won't accept

- Code without DCO sign-off
- Code without tests
- Cosmetic-only changes (whitespace, reformatting) unless 
  accompanied by other work
- Breaking changes without prior discussion
- Dependencies that aren't AGPL-compatible

## How we review PRs

- We aim to respond within 7 days
- PRs typically need 1-3 rounds of revision
- Reviewers may suggest changes; please discuss disagreements 
  rather than re-pushing without addressing feedback

## Questions?

Email hello@uptimepage.dev or open a GitHub Discussion.
