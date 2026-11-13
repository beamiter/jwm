# Prioritize a published authenticated peer close

The receiver may already have decoded and published `SessionClose` while a
final frame still occupies the viewer queue. Continuing the next viewer-loop
iteration can emit an ACK or another protocol record to a closed socket and
mask the authenticated close reason with a secondary transport error.

The viewer now consumes a direct `PeerClosed` at the start of each iteration,
before control checks and outgoing work. Other receiver errors remain in their
existing path. Session cleanup still runs, and genuine cleanup failures remain
reported with the original close code. Authentication and wire format do not
change.

This does not guarantee delivery of a close record or eliminate a close arriving
mid-iteration. The targeted regression covers published-close priority,
non-close error preservation and cleanup-failure classification. The extended
workload matrix separately exercises real TCP and software-GL cleanup under
presentation-credit pressure.
