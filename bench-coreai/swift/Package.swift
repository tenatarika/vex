// swift-tools-version: 6.0
// Swift Package Manager manifest for the Core AI side of the bench.
// macOS 27+ is required because Apple's Core AI framework first ships there.
// The host machine running this bench MUST be macOS 27 + Xcode 27.

import PackageDescription

let package = Package(
    name: "CoreAIBench",
    platforms: [
        // Bumping below 27.0 won't link — Core AI symbols are unavailable.
        .macOS("27.0")
    ],
    products: [
        .executable(name: "CoreAIBench", targets: ["CoreAIBench"])
    ],
    dependencies: [
        // Apple's runtime utilities for loading .aimodel resources.
        // Pinned at branch-tip while the package is pre-1.0; update once they
        // cut a tagged release.
        // .package(url: "https://github.com/apple/coreai-models.git", branch: "main"),
    ],
    targets: [
        .executableTarget(
            name: "CoreAIBench",
            dependencies: [
                // TODO: re-enable once the apple/coreai-models Swift package
                // declares its product name. The README links the export
                // recipe that produces the .aimodel folder this bench
                // consumes.
                // .product(name: "CoreAIModels", package: "coreai-models"),
            ]
            // No `resources:` block. SwiftPM forbids resources outside the
            // package root, so `.copy("../../corpus.json")` would refuse to
            // build. Instead, main.swift reads corpus.json at runtime from
            // BENCH_CORPUS env or "../corpus.json" relative to CWD. This
            // keeps the corpus single-sourced (Rust + Swift identical input).
        )
    ]
)
