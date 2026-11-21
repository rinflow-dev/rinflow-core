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
| `{"window":"front"}` | Show the window in front there: the one the user works in while driving that computer. |
| `{"window":<id>}` | Show the window of that ID (one the computer told) again, and bring it to the front there: the pointer on the picture reaches whatever is on top. |

The computer says, with `DeviceSettings`:

| Report | Meaning |
| --- | --- |
| `{"window":{"id":…,"title":…,"x":…,"y":…,"width":…,"height":…}}` | The window it shows. `id` is a number of its own choosing that stays the window's. The place is on its desktop, in the logical points of the arrangement ([architecture.md](architecture.md#arrangement)): what of the window is seen, which is what the picture is of. |
| `{"window":null}` | It shows none. |

- **When.** Whenever that changes (another window, the window moved or resized, closed), and in answer to every ask, changed or not. An ask that finds nothing to show (the desktop in front, a window since closed) changes nothing, and is answered all the same.
- **Not every report is an answer.** A device also says its settings when a session starts, so a report can be older than an ask. A watching device that waits for an answer therefore takes only a report with a window for one, and gives up after a moment otherwise.
- **Only when asked.** A window is shown because a peer asked for it, never because one watches. When the last viewer stops watching, or the window is closed, none is shown.
- **One at a time.** A source has one picture ([media.md](media.md)), so one window for all who watch.

## The picture

Frames of the window alone, whatever lies over it there, with the computer's cursor while it is on the window. Its size in pixels is the picture's; a key frame starts with the parameter sets, so a resized window is a new size the decoder learns from the next key frame. A window where nothing moves sends nothing, and a new viewer gets the last picture again as a key frame.

## Driving it

The pointer goes as a share of the picture, turned into a position on the computer's desktop with the window's place: `x + share.x × width`, `y + share.y × height`, sent as an absolute move.

- **The place holds through a drag.** Dragging the window by its title moves what the picture is of. A position worked out with the new place would move the pointer again, and the window with it, and so on. So the watching device looks the place up when the pointer moves with no button down, and keeps it from a press to its release.
- **The window must be on top there.** The watching device asks for it by ID when its user turns to the picture. Whether that succeeds is up to the computer's system; a click on the picture lands on whatever is on top.
- **One controller at a time** holds as ever: driving the computer from the picture is driving it.

## Not yet

- The window stays where it is on the computer's own screen. Moving it off that screen (onto a display that is not there) and dragging it across a screen edge come later.
- Any paired device may ask. There is no setting for it, and nothing shows on the computer that a window is being watched.
- Menus and other windows that open beside the one shown are not in the picture.
