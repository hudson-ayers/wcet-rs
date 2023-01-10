# Tock WCET Analysis Tooling

This repository contains code that uses a modified version of the Haybale symbolic execution
engine to find longest paths (in LLVM IR) through Rust code.

## Status

The code currently in this repository is rough research code, and has only been tested on a fork
of Tock that modifies the build system to generate the necessary LLVM IR and MIR files. This Tock
fork also contains some modifications to the Tock kernel necessary to bound symbolic execution. In general,
the changes to the Tock kernel should not modify the behavior, and instead should merely pass additional
information to the compiler (such as communicating loop bounds via assert statements). The Tock fork
is contained in a git submodule of this repository, as Tock uses a non-Cargo based build system and is not
published as a crate.

This code uses a fork of Haybale that adds Rust-specific symbolic execution support,
allowing this tool to effectively handle code with dynamic dispatch of trait method calls.
This fork also makes some assumptions that are true for Tock, but not true for all Rust code:

- all linking is static, such that all implementations of a trait are available at compile time

- virtualizers in Tock do not call their own instances of a trait (e.g. if `<MuxAlarm as Alarm>::fired()`
  contains a call to `<dyn Alarm>::fired`, we assume that that concrete method being called is any
  implementation of the `Alarm::fired()` trait method except `<MuxAlarm as Alarm>::fired()`.
  Currently, this is broadly enforced for *all* trait object methods, as it was easiest to implement that
  way and I do not believe that any trait object methods in Tock include recursive calls to themselves.
  However, this can, and probably should, be updated to only work this way for virtualizers specifically,
  to reduce the scope of assumptions required.

This tool works for a set of system calls and interrupt handlers on certain Tock boards, but cannot yet
find longest paths for all system calls and interrupt handlers. Deriving actual execution times from
these longest paths requires additional tooling, such as verilator or a mechanism for converting LLVM IR
to platform-specific assembly, and using published instruction timing for that assembly. Currently, this
is future work.

As of 1/9/2023 much of the code that originally lived in this repository has been moved into the Haybale fork
which this repository depends on. I have found as this work has progressed that the API exported by upstream
Haybale is not flexible enough to support many of the WCET-finding specific optimizations which this work
requires. Once this tool is in a more stable state I plan to move much of that functionality back into this
repository after modifying Haybale enough to provide the flexibility I need.

## Installation + Setup

Using this tool requires installing several system packages which are necessary for Haybale - specifically
a shared library version of boolector-sys and a shared library version of llvm-sys. Instructions for installing
boolector can be found here: https://github.com/boolector/boolector. However, boolector-sys requires that it be installed
as a shared library, so make sure to pass the `--shared` flag to `./configure.sh`. Don't worry about building with Python
bindings. Also, you need to run `make install` after running `make`, and need to run `sudo ldconfig` after installing.

Instructions for installing LLVM can be found here: https://gitlab.com/taricorp/llvm-sys.rs ,
but they are pretty confusing, so here are some instructions that worked for me on Ubuntu 20.04:

```bash
wget https://apt.llvm.org/llvm.sh
chmod +x llvm.sh
sudo ./llvm.sh 13
sudo apt install zlib1g-dev
sudo ldconfig
```
(there may be additional packages you need to install).

This tool will automatically build the Tock board you want to analyze. However, if building fails,
you must enter the tock submodule, and run `make` in the directory of the board you want to analyze.
This may require additional installation steps, see the README of the Tock repository for additional information
if `make` fails.
This tool has been tested on the following boards:
- Imix
- Hail
- Nordic Boards (nrf52840dk, etc.)
- OpenTitan
- Redboard Artemis Nano (simplest, good for getting started)

If you are analyzing a board not present in the upstream Tock repository, you may need to manually
set the target directory so that the tool can find the LLVM bitcode.

You can choose a set of functions for analysis using the command line options to this tool.

Finally, run the tool using `cargo run -- <options>`. The results for each function will placed in a different text file in the root of the directory.
For runs that fail, the results file will contain the error that led to the failure.

## Current Soundness Limitations
The optimizations made by this tool currently make several assumptions which make it possible that this tool returns
longest path results which are not actually the longest paths through the function in question. A list of these limitations
follows, alongside some of my short-or-long-term plans for resolving each.

1. Trait object dispatch: I don't unconstrain variables touched by every path,
   so local greedy optimization is incorrect. Solution: reimplement to duplicate the
   logic in symex_switch. This will strictly harm performance so I am waiting
   to do it until I have worked out the bigger remaining issues – this one is
   actually straightforward, other than that it might interact poorly with my
   existing function call based retry logic, I don't want to retry one of these
   branched calls, only calls with a single known target. Upside of this is
   being able to get rid of the whole hacky cache storing the longest path
   deal.

2. Timeout/retry mechanism: multiple issues
  - Greedy selection of longest path through that specific function, without
    unconstraining any variables touched by other paths
  - even for the path we do take, we are throwing away constraints that it
    accumulates throughout, on local variables, memory, and global allocations
  - I do not verify that the backtrack point I am returning to is "above" (in a
    CFG) the point at which I failed – only that when I failed I was not in the
    failing function. Solution: verify that the
    fn_to_clear is nowhere to be found in the entire callstack of the backtrack
    point. This will limit the number of failures which we are capable of
    handling, but probably not by much, and removes any potential issues with
    recursion or branches internal to the failing function before the failure.
  - haven't confirmed my path appending is correct

3. The path with the most LLVM-IR instructions is not necessarily the path
which will require the most cycles to execute on a given platform. An initial
optimization to improve this would just be to count the length of paths by
ignoring LLVM intrinsics which do not generate CPU instructions (e.g. debug
intrinsics). A more complete solution would allow users to associate a cycle
count with each LLVM instruction. A complete solution requires compiling the
LLVM-IR to CPU instructions for each path and comparing those lengths.

