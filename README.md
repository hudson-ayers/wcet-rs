# Tock WCET Analysis Tooling

This repository contains code that uses a modified version of the Haybale symbolic execution
engine to find longest paths (in LLVM IR) through Rust code.

The code currently in this repository is rough research code, and has only been tested on a fork
of Tock that modifies the build system to generate the necessary LLVM IR and MIR files. It relies
on a fork of Haybale that adds Rust-specific symbolic execution support, allowing this tool to effectively
handle code with dynamic dispatch of trait method calls.

Eventually these forks will be added as git submodules.

This tool works for a set of system calls and interrupt handlers on certain Tock boards, but cannot yet
find longest paths for all system calls and interrupt handlers. Deriving actual execution times from
these longest paths requires additional tooling, such as verilator or a mechanism for converting LLVM IR
to platform-specific assembly, and using published instruction timing for that assembly.
