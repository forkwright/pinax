# Security policy

## Reporting a vulnerability

Report security issues to cody@forkwright.com. Include a description of the issue, reproduction steps if applicable, and any relevant version or environment details.

Expected response: acknowledgement within 72 hours. No bug bounty program.

## Scope

Phase 01 has landed: lexis (the six-type value system) and pinax (pager, buffer pool, B+tree) ship implementation code; hypomnema and phylaxis remain empty crates reserving their workspace position for later phases. Security reports against shipped implementation code (memory safety, data corruption, and - once those phases land - encryption and auth) are in scope, as are reports against the design itself (threat model gaps, missing requirements, unsafe architectural choices).

## Disclosure

Please allow reasonable time to assess and address the issue before public disclosure.
