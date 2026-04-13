**************
 continuwuity
**************

A community-driven `Matrix <https://matrix.org/>`__ homeserver in Rust
######################################################################

continuwuity is a Matrix homeserver written in Rust.
It's the official community continuation of the `conduwuit <https://github.com/girlbossceo/conduwuit>`_ homeserver.

.. ANCHOR: body

|comp_gh|

|comp_fg|

Why the fork?
~~~~~~~~~~~~~

I make too many changes and PRs, all of them non-breaking and additive (so, yes, it's fully backward-compatible).

I cannot realistically PR all of the improvements I make into the main repo, so I've set up this fork.

The main branch is generally the most stable. Feature branches are best avoided unless talking to me first.

Complement tests have been added, as well functionality for these:

.. code-block:: text

   ✓  tests/msc3890 (9.129s) [Remotely silence local notifications]
   ✓  tests/msc3967 (9.445s) [Do not require UIA when uploading cross-signing keys]
   ✓  tests/msc4155 (19.007s) [Invite filtering]
   ✓  tests/msc4222 (11.685s) [Adding `state_after` to `/sync`]

   ✓ MSC3266 [Room summaries]
   ✓ MSC3890 [Remotely silence local notifications]
   ✓ MSC4289 [Explicitly privilege room creators]

   TODO:

   - MSC4108 [QR Code login]
   - other complement failures relevant to continuwuity

Chats to join (my fork)
~~~~~~~~~~~~~~~~~~~~~~~

* `#general:nutra.tk <https://matrix.to/#/!tgmfqAWaBc978M80V9:nutra.tk>`_ (General chat)
* `#matrix-meta:nutra.tk <https://matrix.to/#/!DEQ3Gb1XlHZHTgNHNw:nutra.tk>`_ (Matrix/Meta talk)
* `#matrix-testing:nutra.tk <https://matrix.to/#/!D1J4GsCJBfrgJ0aXT0:nutra.tk>`_ (Testing room)

.. Substitutions for Badges

.. |comp_gh| image:: https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fgamesguru%2Fcontinuwuity%2F_metadata%2Fbadges%2Fbadge-main.json
   :target: https://github.com/gamesguru/continuwuity/actions/workflows/complement.yml?query=branch%3Amain
   :alt: Complement Tests (GitHub)

.. |comp_fg| image:: https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fgamesguru%2Fcontinuwuity%2F_metadata%2Fbadges%2Fbadge-main-upstream.json
   :target: https://forgejo.ellis.link/gamesguru/continuwuity/actions?workflow=complement.yml&actor=0&status=0
   :alt: Complement Tests (Forge)
