# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project doesn't care about versioning.

## [Unreleased]

## [1.1.0] – 2021-04-22

### Added

- Support AUR VSCode binary (see [GH-6], thanks [SantoJambit]).
- Support for storage format of VSCode 1.55 (see [GH-5], thanks [SantoJambit])

### Fixed

- Exit with failure if the bus name is already owned by another process.
- Substitude prefix in service files (see [GH-4], thanks [SantoJambit]).
- Maintain order of workspaces in results (see [GH-7]).

[SantoJambit]: https://github.com/SantoJambit
[GH-4]: https://github.com/lunaryorn/gnome-search-providers-vscode/pull/4
[GH-5]: https://github.com/lunaryorn/gnome-search-providers-vscode/pull/5
[GH-6]: https://github.com/lunaryorn/gnome-search-providers-vscode/pull/6
[GH-7]: https://github.com/lunaryorn/gnome-search-providers-vscode/pull/7

## [1.0.0] – 2021-04-18

Initial release with support for workspaces of Code - OSS from Arch Linux.

[Unreleased]: https://github.com/lunaryorn/gnome-search-providers-vscode/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/lunaryorn/gnome-search-providers-vscode/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/lunaryorn/gnome-search-providers-vscode/releases/tag/v1.0.0
