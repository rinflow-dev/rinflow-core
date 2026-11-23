# A window as a picture

A computer can show one of its windows on another device: the program keeps running where it is, and the other device shows the window in a window of its own and uses it from there. The first pair is a Windows PC whose window shows on a Mac (`rinflow-windows` `agent/src/picture`, `rinflow-macos` `Rinflow/Viewer`).

The core carries all of it without knowing: nothing here is a message or a call of its own.

| What | Carried by |
| --- | --- |
| The window's picture | The media link ([media.md](media.md)): the computer is a source (`media_source`), the other device watches. |
| Which window, and where it is | The devices' settings (`DeviceSettings` and `DeviceSettingsChange`, capability bit 16; [protocol.md](protocol.md)), under the key `window`. |
| The pointer and keys | Ordinary control: the watching device drives the computer, with absolute positions. |

So the shells of both ends must agree on what follows. A shell that does not know the key `window` ignores it, as with every key of the settings.

## Which window

The watching device asks, with `DeviceSettingsChange`:

| Ask | Meaning |
| --- | --- |
| `{"window":"front"}` | Show the window in front there: the one the user works in while driving that computer. When the system itself is in front for the moment (a notification, a menu of its own), that is the frontmost window of a program. |
| `{"window":"dragged"}` | Show the window being dragged with the mouse there right now. The watching device asks this when its pointer is about to leave the computer with a button held: a window dragged to the edge comes along. |
| `{"window":<id>}` | Show the window of that ID (one the computer told) again, and bring it to the front there: the pointer on the picture reaches whatever is on top. |
| `{"window":{"titled":"<text>"}}` | Show the frontmost window with that text in its title (in capitals or not), wherever it is among the others, also minimized. For a device that knows which program's window it wants without anyone at the computer to bring it to the front. |

An ask may also carry, beside `window`:

- `"screen":{"width":…,"height":…,"scale":…}`: the screen the asker shows the window on, in points, without what it keeps for itself there (a menu bar, a dock), and its pixels to a point. A computer with a monitor nobody looks at makes that monitor its twin (below).
- `"at":{"x":…,"y":…}`, with a window's ID: put the window with its top left corner there, in points from the top left of the monitor it stands on. The asker's user put the window's picture there on its own screen.
- `"size":{"width":…,"height":…}`, with a window's ID: make what is seen of the window that large, in points (a maximized one is restored first). The asker's user resized the window's picture.
- `"hit":{"x":…,"y":…}`, with a window's ID: say what of the window is at that spot, in points from the top left of what is seen of it (outside it for an edge). The asker's user pressed there on the picture, and the asker holds the press until it knows (below).
- `"zoom":true`, with a window's ID: maximize the window, or restore it if it is maximized. The asker's user double-clicked its title bar.
- `"minimize":true`, with a window's ID: minimize the window. The asker's user put its picture away (⌘M on a Mac); the asker's window follows the `minimized` the answer says.

An ask may carry `"ask":<number>` beside `window`, a number of the asker's choosing that it never used before (also not before it restarted: the time will do). The answer then says which ask it answers.

The computer says, with `DeviceSettings`:

| Report | Meaning |
| --- | --- |
| `{"window":{"id":…,"title":…,"x":…,"y":…,"width":…,"height":…,"screen":{…},"minimized":…}}` | The window it shows. `id` is a number of its own choosing that stays the window's. The place is on its desktop, in the logical points of the arrangement ([architecture.md](architecture.md#arrangement)): what of the window is seen, which is what the picture is of. `screen` is the monitor it stands on, as `x`, `y`, `width` and `height` in the same points. `minimized`: it is put away there, and the place is the one it had. |
| `{"window":null}` | It shows none. |
| `"answer":{"ask":<number>,"window":<id>}` beside `window` | The answer to the numbered ask: the ID of the window the ask found (the one now shown), or `null` when it found none (then what was shown before stays). |
| `"cursor":"…"` beside `window` | The shape of the computer's pointer while it shows a window: `arrow`, `text`, `hand`, `crosshair`, `wait`, `progress`, `not_allowed`, `move`, `resize_ew`, `resize_ns`, `resize_nwse`, `resize_nesw`, `help`, or `none` while it is hidden; `custom` for a program's own, with `"cursor_image":{"png":…,"x":…,"y":…,"scale":…}` beside it: the picture (PNG, base64, at most 3400 bytes; an `arrow` instead when it would not fit), the spot of it that points in its pixels, and its pixels to a point. The picture then has no pointer in it: the watching device shows its own over the picture, in that shape, so that it moves at once. A shape it does not know is `arrow`; a computer that does not say has its pointer in the picture. |
| `"window_icon":{"png":…,"scale":…}` beside `window` | The icon of the window shown: a PNG (base64, at most 3400 bytes: its small icon where its large one would not fit) and its pixels to a point. Not beside a `cursor_image`, which takes the room, nor where the settings would be longer than the core carries: a watching device keeps the icon it was told until another window is shown, and shows it where it shows the window's picture put away (a Dock). |
| `"locked":true` beside `window` | The window shown cannot be seen: its lock screen is in front of it, or the computer asks something on a desktop of its own (the password, an administrator's consent). Clicks on the picture reach the lock screen, not the window, and no pictures come until it is unlocked; the window stays the one shown, and pictures come again from a key frame. The watching device says so on the picture. |
| `"hit":"…"` in `answer` | For an ask with `hit`: what is at that spot, as the window itself says it to its system (a program that draws its own title bar knows where that is): `caption`, an edge (`left`, `right`, `top`, `bottom`, `top_left`, `top_right`, `bottom_left`, `bottom_right`), a button (`minimize`, `maximize`, `close`), `client` for what the window shows, or `nowhere`. A word the asker does not know is `client`. |

- **When.** Whenever that changes (another window, the window moved or resized, closed), and in answer to every ask, changed or not. An ask that finds nothing to show (no program's window open, a window since closed) changes nothing, and is answered all the same.
- **A press waits on the answer.** A computer answers an ask with `hit` for the window it shows at once, before anything else it is doing (a new encoder can take most of a second), with its settings as they were and the answer.
- **Not every report is an answer.** A device also says its settings when a session starts and whenever its window moves, so a report can be older than an ask, or about something else. A watching device that waits for an answer numbers its ask and takes the report with that number for the answer; it gives up after a moment otherwise.
- **Not while it is locked.** A computer shows nothing from behind its lock screen: an ask then finds no window, and a window being shown sends no pictures (it says `"locked":true`) until the computer is unlocked.
- **Only when asked.** A window is shown because a peer asked for it, never because one watches. When the last viewer stops watching, or the window is closed, none is shown.
- **One at a time.** A source has one picture ([media.md](media.md)), so one window for all who watch.

## Where the window stands meanwhile

The window stays on the computer's own desktop: its program must keep drawing it, and the pointer must reach it. Where the computer has a monitor nobody looks at (a virtual display driver's), the window is moved onto that one while it is shown and back where it stood afterwards: off the screens people look at, and not in the way of what happens there. It should look there as it did where it stood: a monitor of another scale has the program draw the window anew, with other pixels, so the computer gives that monitor the scale of the screen the window comes from where it can. Such a monitor is not among the displays the computer tells its peers (`ScreenInfo`), so no pointer wanders onto it; positions on it still mean something to the computer, which is all driving the window needs. Without such a monitor the window stays where it is, on a screen someone may be looking at.

**The window as a window of the watching device.** A watching device can show the picture without a frame of its own, as the window it is: at the place on its screen that the window has on its monitor (`x`, `y` less the corner of `screen`), as large, put away while it is `minimized`. Where it stands and how large it is, is then the watching device's: a press of the main button on the picture is held while the device asks what is there (`hit`). By the title bar it moves the picture's window itself, by an edge it resizes it itself (the edge lies just outside what is seen of the window, so the watching device takes the pointer a little outside the picture too), at once, without waiting on the computer, and tells it afterwards (`at` and `size`), so that the window stands there as it stands here; a double click on the title bar is `zoom`. A press anywhere else, the window's buttons among it, goes to the computer as it would have, and the window's place follows what happens there (maximized, minimized, resized by its program). For that the monitor nobody looks at is made the twin of the asker's `screen`: as large in points, and of as many pixels to a point as the driver has a size for, so that what fills it fills that screen and the window is no larger or smaller there than it is shown.

A window taken while it was dragged is let go at a screen edge when the pointer leaves; the computer then puts all of it back on a screen (or on the spare monitor).

## The picture

Frames of the window alone, whatever lies over it there, and with its menus and pop-ups as far as they lie over it (where the computer's system can capture them with it); with the computer's cursor while it is on the window only from a computer that does not say its shape (`cursor`). A window where nothing moves sends nothing, and a new viewer gets the last picture again as a key frame.

**Frames of one size, and the windows in them.** The frames are as large as the monitor the window is on (the largest it can be), so that resizing or maximizing it never begins the encoder anew; black where no window is. Where the window stands on a monitor nobody looks at, the program's other windows there are in the frames too, each where it is (its menus, dialogs and tooltips: they open beside it, and a window the shown one owns that opens on another monitor is brought there, beside it); elsewhere the window alone, at the top left. Each frame says which windows are where in it: an SEI message `user_data_unregistered` (payload type 5) before its first slice, of the identifier `RinflowWindows` followed by the bytes 0 and 1, the number of windows (one byte, at most 19), and for each, from the front, the lower 32 bits of its `id` (four bytes) and its left, top, width and height in pixels (two bytes each), all big endian. A decoder passes over it. A watching device shows the window it asked for from its part of the frame, and each other window in a window of its own, placed beside it as it is there; a frame without the message is one picture, all of it.

## Driving it

The pointer goes as a share of the picture, turned into a position on the computer's desktop with the window's place: `x + share.x × width`, `y + share.y × height`, sent as an absolute move.

- **The place holds through a drag.** Dragging the window by its title moves what the picture is of. A position worked out with the new place would move the pointer again, and the window with it, and so on. So the watching device looks the place up when the pointer moves with no button down, and keeps it from a press to its release; one that moves the picture along with the window keeps where the picture was on its own screen for as long, too.
- **The window must be on top there.** The watching device asks for it by ID when its user turns to the picture. Whether that succeeds is up to the computer's system; a click on the picture lands on whatever is on top.
- **One controller at a time** holds as ever: driving the computer from the picture is driving it.

## Not yet

- Dragging the picture back across the edge to send the window home; several windows at once (a source has one picture).
- Any paired device may ask. There is no setting for it, and nothing shows on the computer that a window is being watched.
- Without a monitor nobody looks at, a menu or dialog reaching beyond the window is cut off at its edge, and other windows the program opens are not in the picture. A window of the program's that the shown one does not own (a second window of a browser) stays where it opens.
