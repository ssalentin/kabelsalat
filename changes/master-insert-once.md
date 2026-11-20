### Fixed
- **Master inserts sound like the same effect in a strip (#154):** an effect
  after Master OUT was processed twice per block, once on silence. A Delay
  there played its echoes chopped and at the wrong time, a Limiter let go
  between blocks, and every master insert ran twice. Each insert
  now runs once, on the mix.
