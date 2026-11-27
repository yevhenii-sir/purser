# Versions and upgrades

What a release promises, how often releases come out, and how far each store has been tried. Read it before you depend on Purser, and before each upgrade.

## Status

Purser is **before 1.0** and has **not run a live game yet**.

- Every store is covered by the module's tests against recorded store answers.
- CI runs an end-to-end check of the built module on a real SpacetimeDB server.
- But no store has handled real purchases through Purser so far.

!!! warning
    Try it with test purchases before your players depend on it. Keep a backup (`server/backup.py`) of anything you cannot lose.

| Store | Automated tests | Sandbox purchases | Live purchases |
|---|---|---|---|
| Google Play | yes | not yet | not yet |
| App Store | yes | not yet | not yet |
| Amazon Appstore | yes | not yet | not yet |
| Huawei AppGallery | yes | not yet | not yet |
| Samsung Galaxy Store | yes | not yet | not yet |
| Meta Quest (Horizon Store) | yes | not yet | not yet |
| Steam | yes | not yet | not yet |
| Stripe | yes | not yet | not yet |
| Xsolla | yes | not yet | not yet |
| Telegram Stars | yes | not yet | not yet |

- **Sandbox purchases**: test purchases of a real build in the store's test environment (license testers, the App Store sandbox, test cards). They go from the client to the reward in the game, refunds and renewals included.
- **Live purchases**: a released game's players paying real money.

A store moves on once that has worked end to end. The changelog says when.

## Before 1.0

- **A minor release** (`0.x.0`) may change tables in a way SpacetimeDB cannot migrate in place. Its notes then start with **Upgrade: publish with `--delete-data`**, and say how to keep the data with `backup.py`.
- **A patch release** (`0.x.y`) always publishes in place.
- **The Unity package and the SDKs of other engines** ship with the server: update them together. A client of another minor version may send fields the server no longer reads.
- **1.0** comes when Google Play and the App Store have handled the live purchases of a released game for at least a month without a lost purchase.

## From 1.0

- **Every release publishes in place: never `--delete-data`.** Schema changes add tables and columns (with defaults). Something removed is first deprecated in a release of its own.
- **The server accepts the requests of the previous minor version's Unity package and SDKs.** So the server can be upgraded first, and the game's next build can follow.
- **Breaking changes** wait for a major version (`2.0`), with a migration guide.

## How often

- **Minor releases** come at most **once a week**, the week's work together.
- **Fixes** come as patch releases when needed, security fixes right away.
- **Every release** has a section in [CHANGELOG.md](https://github.com/yevhenii-sir/purser/blob/main/CHANGELOG.md). It starts with:
    - **In short**: two or three lines on what changed;
    - **Upgrade**: in place, or `--delete-data` and how to keep the data;
    - then the details.

    That section, with the Unity package's, is the GitHub release's notes.

## Upgrading

1. Read the release's **Upgrade** line ([releases](https://github.com/yevhenii-sir/purser/releases)).
2. Ask the server whether the new module publishes in place: run `python publish_check.py` from `server/` ([self-hosting](self-hosting.md#upgrading)).
3. Publish:
    - **In place:** publish the new `purser.wasm` to the same database.
    - **With `--delete-data`:** back up, publish, restore (`backup.py backup` / `restore`, the same section).
4. Update the Unity package (or the SDK) to the same version in the game's next build.
