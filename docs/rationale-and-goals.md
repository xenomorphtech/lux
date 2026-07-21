# Lux Rationale And Goals

This document states what Lux is for, why it is being built this way, and what properties matter most.

## Rationale

Lux is not just a compiler for turning source files into dead artifacts. The intended system is a live programming environment for building, evolving, inspecting, and running long-lived software systems.

The target is not limited to small reductions or one-shot evaluations. Lux is expected to support:

- livecoding of systems that stay up
- incremental evolution of running software
- parallel versions of similar systems with small behavioral differences
- inspection and debugging of deployed systems
- typed access to network and service capabilities

The current `eval` and `run` paths are useful, but they are support tools. They are not the end goal.

## Core Thesis

The core idea is:

- code artifacts are immutable and content-addressed
- names are resolved through mutable namespaces and frozen snapshots
- functions and types are edited as atomic namespace symbols, never as files
- capabilities are admitted explicitly through the typed environment
- long-running systems should be inspectable and replaceable without losing provenance

This is meant to support a live-code model rather than a file-build-deploy model.

## Primary Goals

### 1. Live Code, Not Dead Code

Lux should make code something that can be:

- compiled and inspected before publication
- published into a namespace
- snapshotted into an immutable environment
- deployed as a long-running system
- upgraded by changing bindings and snapshots rather than overwriting a single global version

### 2. Immutable Artifacts, Mutable Name Resolution

Functions compile to immutable, hash-addressed artifacts.

Human-readable names should not be the runtime identity of the code. Instead:

- names are for authoring and introspection
- hashes are for executable identity
- namespaces decide what a name means now
- snapshots preserve what a name meant at a specific point in time

This allows multiple nearby versions of the same system to coexist.

### 3. Typed Capability Admission

The intended security boundary is the language and type/capability system.

The goal is not to rely primarily on:

- syscall / network isolation
- per-job OS sandboxing
- ad hoc runtime blocking of ambient authority

Instead, the goal is to make unsafe or unavailable powers absent from the compile-time environment entirely.

Code should not be able to typecheck against capabilities it was not given.

Operational limits such as timeouts and output caps are still useful, but they are not the fundamental model.

### 4. Long-Running Robust Systems

Lux is intended to support robust networked systems, not just pure evaluations.

That implies support for:

- long-running supervised instances
- lifecycle inspection
- restarts and crash visibility
- deployable service roots
- typed networking and service capabilities
- side-by-side versions during upgrades and experiments

### 5. Empirical Inspectability

The system should be operable by inspection.

Users should be able to ask:

- what is published in this namespace?
- what changed between generation 4 and 5?
- what exactly is in snapshot 12?
- what artifact hash is this service actually running?
- what did this execution resolve to?

This is why namespaces, snapshots, diffs, execution records, and metadata matter.

## Near-Term Goals

The current near-term direction is:

- keep content-addressed function artifacts
- keep immutable per-symbol source revisions for functions and types
- keep namespace and snapshot management explicit
- support dry-run compilation against snapshots
- support selective publication of symbols
- support execution with bounded operational limits
- keep an audit trail of executions
- add deployment and long-running instance management

## Long-Term Goals

Longer term, Lux should grow into a platform for:

- typed live deployment of systems
- explicit capability-granted networking and service access
- parallel operation of multiple system generations
- upgrade and rollback through namespace/snapshot control
- debugging and inspection grounded in artifact identity

## Non-Goals

Lux is not trying to be:

- a traditional build pipeline centered on mutable named modules
- a runtime whose main security story is OS sandbox wrappers
- a system where reproducibility depends on ambient global state
- a tool only for toy reductions or one-off code execution

## Design Consequences

These goals imply several architectural choices:

- artifact identity must be based on normalized content
- authoring APIs must compare-and-swap one namespace symbol rather than rewrite an aggregate source blob
- publication must be separate from compilation
- snapshot-based execution must be first-class
- deployments and instances should become explicit runtime objects
- introspection metadata must be preserved even when executable names are hashes

## Current Status

The repo already has the beginnings of this model:

- content-addressed per-function artifacts
- namespace publication and snapshotting
- dry-run compilation
- selective symbol publication
- execution records and diffs
- minimal deployment and instance lifecycle control
- service endpoints for inspection and execution

The current deployment model is still only a first step:

- deployments and instances can be created and inspected
- instances can be stopped
- process lifecycle is persisted

What is still missing is full supervision, restart policy, richer event streams, and typed network/service capability surfaces for robust long-running systems.
