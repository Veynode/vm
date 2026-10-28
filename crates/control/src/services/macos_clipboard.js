// The browser desktop's clipboard for a macOS container, run by the relay
// (desktop_stream.rs) as `osascript -l JavaScript` in the Guest's login
// session over SSH: nothing is installed in the Guest. Lines are those of the
// Linux agent's clipboard (guest/veynode-agent/clipboard_x11.go):
//   in  {"clipboard":"text"}, {"clipboard_watch":bool}, {"clipboard_get":true},
//       {"poll":true} (the relay's tick: report a Guest copy while watching);
//       ASCII only, other characters as \u escapes
//   out {"event":"clipboard","text":"..."} or {"event":"clipboard","error":"..."}
// argv[0], when given, names a private pasteboard instead of the general one (tests).
ObjC.import('AppKit')

function run(argv) {
  const board = argv.length ? $.NSPasteboard.pasteboardWithName(argv[0]) : $.NSPasteboard.generalPasteboard
  const input = $.NSFileHandle.fileHandleWithStandardInput
  const output = $.NSFileHandle.fileHandleWithStandardOutput
  const limit = 200 * 1024
  const send = (event) => output.writeData($(JSON.stringify(event) + '\n').dataUsingEncoding($.NSUTF8StringEncoding))
  const current = () => {
    const text = board.stringForType($.NSPasteboardTypeString)
    if (text.isNil()) return { event: 'clipboard', error: "the Guest's clipboard holds no text" }
    if (text.lengthOfBytesUsingEncoding($.NSUTF8StringEncoding) > limit) {
      return { event: 'clipboard', error: `the Guest's clipboard is over ${limit} bytes` }
    }
    return { event: 'clipboard', text: text.js }
  }
  let pending = ''
  let seen = Number(board.changeCount)
  let watching = false
  for (;;) {
    const data = input.availableData
    if (Number(data.length) === 0) return
    pending += $.NSString.alloc.initWithDataEncoding(data, $.NSASCIIStringEncoding).js
    let newline
    while ((newline = pending.indexOf('\n')) >= 0) {
      const line = pending.slice(0, newline)
      pending = pending.slice(newline + 1)
      let message
      try {
        message = JSON.parse(line)
      } catch (error) {
        continue
      }
      if (typeof message.clipboard === 'string') {
        board.clearContents
        board.setStringForType($(message.clipboard), $.NSPasteboardTypeString)
        // Our own paste is not a Guest copy.
        seen = Number(board.changeCount)
      }
      if (typeof message.clipboard_watch === 'boolean') {
        watching = message.clipboard_watch
        seen = Number(board.changeCount)
      }
      if (message.clipboard_get === true) send(current())
      if (message.poll === true && watching && Number(board.changeCount) !== seen) {
        seen = Number(board.changeCount)
        send(current())
      }
    }
  }
}
