# Albion Clientless Port

The guest-account and guest-character milestones are complete in the database-native
namespace `albion.clientless`. Lux created a guest account, logged in through OP5,
handled the Xign-capable continuation, submitted CreateCharacter op12, parsed the
server-assigned tutorial cluster, and stored the resulting state as encrypted
namespace resources.

All domain-specific behavior is Lux source in SQLite. Rust contains only generic
storage, compilation, cryptography, networking, capability, protected-resource,
and MCP machinery; it contains no Albion-specific protocol names, constants,
fixtures, or workflow logic.

## Current namespace

At the completed character milestone, generation 72 / snapshot 72 contains the Lux
implementations for:

- Photon framing and command parsing
- GP parameter encoding and typed response decoding
- Diffie-Hellman, AES framing, hashing, and random material through typed generic
  capabilities
- UDP login handshake and guest-account workflow
- OP5 login and typed response parsing
- Xign event decoding, proof validation, and responder exchange over generic
  `net.tcp`
- CreateCharacter op12 construction, response parsing, and retry/session flow
- device/session identity generation
- encrypted identity, credential, login, and character resource writes
- deterministic protocol fixtures and live workflow entry points

The legacy changeset provenance and subsequent port work are indexed as atomic
function and type symbols. Future development uses `lux.get_symbol` and
`lux.put_symbol`; it does not retrieve or patch an aggregate changeset source blob.

The successful live guest-character execution is recorded as
`ea72a3ca65e3974ac4b2f8bb3675928aea0c1b5abca0eb9e486f7e22de096246`.
Its response exposed resource metadata only. The protected resource heads include:

- `guest.identity`
- `guest.credentials`
- `guest.login`
- `guest.character`
- `xign.responder`

Secret values are not printed, returned by MCP, or stored in files as the source of
record.

The redacted `guest_character_resource_is_complete` verifier confirms that the
encrypted character document contains a non-empty name and cluster. The normal
resource API still refuses plaintext reveal unless explicitly enabled for local
administration.

## Next port stage

The next domain work remains Lux-only:

1. Port character selection and login-server game-endpoint discovery.
2. Port the game-server handshake, authentication, and initial join.
3. Model reconnect, retry, and session lifecycle as inspectable Lux symbols and
   executions.
4. Add explicit database/key backup and restore tooling before relying on the store
   as the only production copy.

Each function or type is replaced atomically with an expected namespace generation
and symbol revision. A function edit rebuilds only its transitive artifact callers;
a type edit may rebuild all executable artifacts. Lux commits all derived bindings
together as one namespace generation while the caller still edits only the selected
symbol.
