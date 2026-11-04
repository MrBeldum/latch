// onscreen.js <pid>: the on-screen windows (current Space) the process owns, as WxH.
function run(argv) {
  ObjC.import('CoreGraphics');
  const pid = parseInt(argv[0]);
  const list = ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0));
  const out = [];
  for (let i = 0; i < list.count; i++) {
    const w = list.objectAtIndex(i);
    if (w.objectForKey('kCGWindowOwnerPID').intValue !== pid) continue;
    if (w.objectForKey('kCGWindowLayer').intValue !== 0) continue;
    const b = w.objectForKey('kCGWindowBounds');
    out.push(b.objectForKey('Width').intValue + 'x' + b.objectForKey('Height').intValue);
  }
  return out.length ? 'onscreen ' + out.join(',') : 'offscreen';
}
