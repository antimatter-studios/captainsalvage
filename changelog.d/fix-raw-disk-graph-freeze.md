### Fixed

- **A raw-disk scan with thousands of findings no longer freezes the window.** The map painted each batch of evidence by checking every finding against every pixel of the strip, so a scan with 2000 findings spent over two seconds repainting while it ran. It now paints each strip in one pass, and the list of unreadable regions shows 100 at a time, as the evidence list already did.
