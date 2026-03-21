continuwuity
============

A community-driven `Matrix <https://matrix.org/>`__ homeserver in Rust
----------------------------------------------------------------------

continuwuity is a Matrix homeserver written in Rust.
It's the official community continuation of the `conduwuit <https://github.com/girlbossceo/conduwuit>`_ homeserver.

.. ANCHOR: body

|comp_gh|

|comp_fg|

Why does this exist?
~~~~~~~~~~~~~~~~~~~~

The original conduwuit project has been archived and is no longer maintained. Rather than letting this Rust-based Matrix homeserver disappear, a group of community contributors have forked the project to continue its development, fix outstanding issues, and add new features.

We aim to provide a stable, well-maintained alternative for current conduwuit users and welcome newcomers seeking a lightweight, efficient Matrix homeserver.

What is Matrix?
~~~~~~~~~~~~~~~

`Matrix <https://matrix.org>`__ is an open, federated, and extensible network for decentralized communication. Users from any Matrix homeserver can chat with users from all other homeservers over federation. Matrix is designed to be extensible and built on top of. You can even use bridges such as Matrix Appservices to communicate with users outside of Matrix, like a community on Discord.

What are the project's goals?
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Continuwuity aims to:

* Maintain a stable, reliable Matrix homeserver implementation in Rust
* Improve compatibility and specification compliance with the Matrix protocol
* Fix bugs and performance issues from the original conduwuit
* Add missing features needed by homeserver administrators
* Provide comprehensive documentation and easy deployment options
* Create a sustainable development model for long-term maintenance
* Keep a lightweight, efficient codebase that can run on modest hardware

Chats to join (my fork)
~~~~~~~~~~~~~~~~~~~~~~~

* `#general:nutra.tk <https://matrix.to/#/!tgmfqAWaBc978M80V9:nutra.tk>`_ (General chat)
* `#matrix-meta:nutra.tk <https://matrix.to/#/!DEQ3Gb1XlHZHTgNHNw:nutra.tk>`_ (Matrix/Meta talk)
* `#matrix-testing:nutra.tk <https://matrix.to/#/!D1J4GsCJBfrgJ0aXT0:nutra.tk>`_ (Testing room)

Can I migrate my data from x?
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

* Conduwuit: Yes
* Conduit: No, database is now incompatible
* Tuwunel: Generally not
* Grapevine: No, database is now incompatible
* Dendrite: No
* Synapse: No

We haven't written up a guide on migrating from incompatible homeservers yet. Reach out to us if you need to do this!


.. Substitutions for Badges

.. |comp_gh| image:: https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fgamesguru%2Fcontinuwuity%2F_metadata%2Fbadges%2Fbadge-main.json
   :target: https://github.com/gamesguru/continuwuity/actions/workflows/complement.yml?query=branch%3Amain
   :alt: Complement Tests (GitHub)

.. |comp_fg| image:: https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fgamesguru%2Fcontinuwuity%2F_metadata%2Fbadges%2Fbadge-main-upstream.json
   :target: https://github.com/gamesguru/continuwuity/actions/workflows/complement.yml?query=branch%3Amain-upstream
   :alt: Complement Tests (Forge)
