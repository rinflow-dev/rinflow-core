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

An ask may carry `"ask":<number>` beside `window`, a number of the asker's choosing that it never used before (also not before it restarted: the time will do). The answer then says which ask it answers.

The computer says, with `DeviceSettings`:

| Report | Meaning |
| --- | --- |
| `{"window":{"id":…,"title":…,"x":…,"y":…,"width":…,"height":…}}` | The window it shows. `id` is a number of its own choosing that stays the window's. The place is on its desktop, in the logical points of the arrangement ([architecture.md](architecture.md#arrangement)): what of the window is seen, which is what the picture is of. |
| `{"window":null}` | It shows none. |
| `"answer":{"ask":<number>,"window":<id>}` beside `window` | The answer to the numbered ask: the ID of the window the ask found (the one now shown), or `null` when it found none (then what was shown before stays). |

- **When.** Whenever that changes (another window, the window moved or resized, closed), and in answer to every ask, changed or not. An ask that finds nothing to show (no program's window open, a window since closed) changes nothing, and is answered all the same.
- **Not every report is an answer.** A device also says its settings when a session starts and whenever its window moves, so a report can be older than an ask, or about something else. A watching device that waits for an answer numbers its ask and takes the report with that number for the answer; it gives up after a moment otherwise.
- **Not while it is locked.** A computer shows no window from behind its lock screen: an ask then finds none, and a window being shown is shown no more.
- **Only when asked.** A window is shown because a peer asked for it, never because one watches. When the last viewer stops watching, or the window is closed, none is shown.
- **One at a time.** A source has one picture ([media.md](media.md)), so one window for all who watch.

## Where the window stands meanwhile

The window stays on the computer's own desktop: its program must keep drawing it, and the pointer must reach it. Where the computer has a monitor nobody looks at (a virtual display driver's), the window is moved onto that one while it is shown and back where it stood afterwards: off the screens people look at, and not in the way of what happens there. It should look there as it did where it stood: a monitor of another scale has the program draw the window anew, with other pixels, so the computer gives that monitor the scale of the screen the window comes from where it can. Such a monitor is not among the displays the computer tells its peers (`ScreenInfo`), so no pointer wanders onto it; positions on it still mean something to the computer, which is all driving the window needs. Without such a monitor the window stays where it is, on a screen someone may be looking at.

A window taken while it was dragged is let go at a screen edge when the pointer leaves; the computer then puts all of it back on a screen (or on the spare monitor).

## The picture

Frames of the window alone, whatever lies over it there, with the computer's cursor while it is on the window, and with its menus and pop-ups as far as they lie over it (where the computer's system can capture them with it). Its size in pixels is the picture's; a key frame starts with the parameter sets, so a resized window is a new size the decoder learns from the next key frame. A window where nothing moves sends nothing, and a new viewer gets the last picture again as a key frame.

## Driving it

The pointer goes as a share of the picture, turned into a position on the computer's desktop with the window's place: `x + share.x × width`, `y + share.y × height`, sent as an absolute move.

- **The place holds through a drag.** Dragging the window by its title moves what the picture is of. A position worked out with the new place would move the pointer again, and the window with it, and so on. So the watching device looks the place up when the pointer moves with no button down, and keeps it from a press to its release.
- **The window must be on top there.** The watching device asks for it by ID when its user turns to the picture. Whether that succeeds is up to the computer's system; a click on the picture lands on whatever is on top.
- **One controller at a time** holds as ever: driving the computer from the picture is driving it.

## Not yet

- Dragging the picture back across the edge to send the window home; several windows at once (a source has one picture).
- Any paired device may ask. There is no setting for it, and nothing shows on the computer that a window is being watched.
- A menu or dialog reaching beyond the window is cut off at its edge, and other windows the program opens are not in the picture.
