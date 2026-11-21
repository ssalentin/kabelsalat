### Added
- **Save the last minutes after the fact (#215):** kabelsalat keeps the last
  two minutes of the master and of every armed rig's stem in memory. Right-click
  REC and choose "Save the last 2:00" to write them as a take folder, exactly as
  you heard them. Settings → Audio → Keep the last sets the length (1 to 10
  minutes, or off; off by default on machines with under 3 GB of memory). The
  assistant and MCP agents save it with `record_save_last`, and
  `record_status` reports what is kept.
