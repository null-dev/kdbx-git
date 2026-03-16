### Changes to webdav
- When a client commits to their branch, it should still merge/fast-forward their change into the main branch.
  - But it should NOT then fanout the changes from the main branch into the client branches
  - If the merge into main fails, the client's commit should still have been written to the client branch and the client should still be told that the commit succeeded.
- Instead, main is only merged/fast-forwarded into a client's branch when a client reads from their webdav endpoint.
  - If the merge from main fails, it is simply skipped and a warning logged. The client should still be able to read their database.

### Changes to sync-local
- I want to eliminate the case where any database merging is performed on the client side.
- So instead, the client should always expect their branch to have not changed since it last looked at it (though the client should still validate this and exit if this expectation is violated)
- The client should:
  1. Listen for changes to the "main" branch
  2. create a temporary commit that merges the main branch into their branch
  3. write out the new KDBX database derived from the temporary commit to a temporary file.
  4. atomically swap out the KDBX database with the new database in the temp file.
  5. Add the merge commit to it's branch on the server.
     - This step must be interruptible. If the program is terminated in between step 4 & 5, it should repeatedly attempt the operation until it succeeds.

### While making your changes, keep in mind:
The client should never have access to the server's git database, because the git database is unencrypted. So this would entail exposing the client to the unencrypted passwords. This increases the attack surface of the client.

Also, the sync-local client does not need to use the same endpoints as WebDAV. E.g. there can be another endpoint on the server that just creates the temporary merge commit and another endpoint that moves the merge commit onto the client's branch.