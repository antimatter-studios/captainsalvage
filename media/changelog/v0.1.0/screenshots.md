<!-- scripts/release.py sync copies this table into the README under v0.1.0, so image paths are relative to the repository root. -->
Taken from the original damaged test disk, with private file names blurred.

| | |
|---|---|
| ![Recovery overview, with a large figure for data found by name above a breakdown of the disk](media/changelog/v0.1.0/overview.png)<br>The overview says what has been found, and the map shows what explains every part of the disk. | ![A lost film selected, with its hex dump and confidence details](media/changelog/v0.1.0/file-and-bytes.png)<br>Pick any file to read its bytes, see why it got its score, and copy it out. |
| ![Claude answering a question about the selected cluster in the chat sidebar](media/changelog/v0.1.0/ask-claude.png)<br>Select anything and ask Claude, who checks with the same tools the app uses. | ![The Copy out tab, with its destination, salvage drive and confidence floor](media/changelog/v0.1.0/copy-out.png)<br>Copy out checks the destination, and refuses the disk being recovered. |
| ![An unexplained region selected, zoomed on the map, with its bytes showing an MP4 header](media/changelog/v0.1.0/unexplained-region.png)<br>The Unexplained list zooms the map to free space nothing accounts for yet. | ![The recovery overview in dark mode](media/changelog/v0.1.0/overview-dark.png)<br>It follows your Mac's dark mode too. |

On the way to this release, the map changed twice.

| | |
|---|---|
| ![The first version, with a mostly blue map and plain grey panels](media/changelog/v0.1.0/1-first-version.png)<br>The first version worked, but the map hid the 32 GB in use, because each column showed only its largest category. | ![The second version, whose map shows a dark band of in-use data among the blue](media/changelog/v0.1.0/2-proportional-map.png)<br>Next, each map column became a stack showing its real mix, so small things stay visible. |
