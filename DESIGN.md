
# High-level goals

The main use case for this software is as a lightweight text editor, intended
to replace the default text editor in an operating system.
It does not aim for extensibility or to be a full-featured IDE, only for basic
plaintext read-and-edit tasks.
The distinguishing characteristic of this text editor is to support
**fully stutter-free** editing of huge plaintext files, even files that do not
fit as a whole in RAM.

# Low-level goals implied by the high-level goals

The editor should be stutter-free to the furthest extent that it may be.
This means that the main thread should never do *any* I/O, and operations with
potential O(N) complexity (including operations with amortized O(1) complexity
but single-operation O(N) worst case) must be done on a background thread.

Memory usage may be O(N), but the constant must be small enough to allow
efficient access to huge files without huge memory usage.
The current aim is to use memory on the order of hundreds of megabytes to speed
up access to files on the order of terabytes.
Some average-case performance is traded for the worst-case performance to be
good enough for seamless editing.

Ideally, complex features that require loading huge libraries should be linked
on-demand, to allow the main text editor to start up quickly.

# Important concepts

- File: An operating system concept. An 8-bit clean stream of bytes, with a
    defined length.
    Note that some operating systems allow files with no defined length.
    For the purposes of this text editor, files must always have a defined
    length.
    (Note: this might change in the future if this limitation proves easy to
    remove)
- Buffer: An "open file". It may reflect exactly the contents of a file, or it
    may include some unsaved modifications.
    It may be linked to an on-disk file, but otherwise it is completely
    separate.
- Save: To take the contents of a buffer and write them out to a file.
    In other words, it synchronizes a file with the contents of a buffer.
    This operation is O(N) in the worst case.
- Persist: To store enough data about a buffer to allow the editor to
    reconstruct the buffer after a restart, assuming the backing file was not
    modified.
    This operation may still be O(N), but a lot faster than saving.
- File offset: An absolute offset into the byte stream of a file.
- Virtual offset: An absolute offset into the byte stream of a buffer.
- Spatial coordinates: A position expressed as a line number and an horizontal
    distance.
    Horizontal distances are real numbers, and are expressed in units such
    that `1` is equal to one font-height.
    Line numbers may be real numbers or optionally constrained to be integers.
    In other words, both `x` and `y` coordinates of spatial coordinates are in
    the same units.
    Spatial coordinates require an origin, and in general there is no absolute
    file origin, so spatial coordinates are only useful as a concept rather
    than in practice.
    The role of spatial coordinates is replaced by "file positions".
- Spatial delta: The difference between two spatial coordinates.
- Buffer position: A tuple of a virtual offset and a spatial delta.
    The represented position is given by applying the given spatial delta to
    the spatial coordinate of the given virtual offset.
- Buffer rect: A tuple of a buffer position and a spatial delta.
    Represents a rectangle of the file, such as a screen scrolled into the
    file.
- Layout delta: Represents the difference between two points in the file as a
    number of lines and a trailing horizontal distance.
    For example, the distance between the last character and the first
    character in the next line is one line and zero horizontal characters.
    This constrasts with the spatial delta.
    The spatial delta between the last character and the first character of the
    next line is one line and a negative amount of characters that depends on
    the length of the first line.
    The advantage of layout deltas is that the layout delta between two offsets
    depends only on the characters between the two offsets.
    This property is useful to calculate spatial deltas between arbitrary
    offsets.

# Operations that we want to do quickly (O(log N)) on the buffer

## Query operations

1. Determine the spatial delta between two offsets, if it is available.
2. Determine the nearest offset given a base offset and a spacial delta.
    There might not be enough data loaded to determine the exact result offset.
    In this case, produce an error with information and an approximate result.
    At least three rounding modes should be provided: floor, round and ceil.
    Additionally, the spatial distance between the resulting offset and the
    original base offset should be provided as part of the result.
3. Given a base offset, determine the lowest offset such that everything
    between this offset and the base is mapped (ie. we can determine distances
    between offsets in this range, possibly after loading the local
    neighborhood of both offsets).
    Offer a similar operation for the highest offset.
    The range between the lowest and highest offsets is called the "mapped
    neighborhood".
4. Determine a lower bound on the maximum width of a line in the mapped
    neighborhood.
    This property is important for horizontal scrollbars, yet it is difficult
    to implement.
5. Iterate over characters starting from an offset (forward and backward).

## Update operations

1. Delete a range of data (aligned to char boundaries), shifting everything
    after the range to the left.
2. Insert a range of data (aligned to char boundaries), shifting everything
    after the range to the right.
    The range might be backed by a file, meaning that the actual data to be
    loaded is not available, and may not be available for a long time.
    (eg. loading a 30GB file).
    If the data is not available, the range is considered unmapped and any
    distances across or within the range are uncomputable, but it still uses
    virtual offset space.
    The data might also be backed by RAM, or it may be backed by a file but
    also have a temporarily loaded buffer in RAM, that may be dropped at any
    time.

## Possible future operations

1. Round an offset to a nearby "special offset" in the same mapped
    neighborhood, that allows quicker lookups.
2. Determine the amount of characters between two offsets.
3. Determine the offset given a base offset and a character count delta.
4. Determine the amount of words between two offsets.
5. Determine the offset given a base offset and a word count delta.

# Data structures used to accomplish this

There are two main data structures used to quickly navigate, view and edit a
huge file: the "linemap tree" and the "sparse data store".

## The sparse store

The sparse data store is a relatively simple data structure that loads and
unloads file data into memory, just like `memmap` would do.
The difference with `memmap`, is that the sparse store avoids invisible latency
at all costs.
When an access to unloaded data is executed, the sparse data store fails
quickly, it does not attempt to transparently load the underlying data.
It also has the side effect of working for huge (>2GB) files in 32-bit systems.

The sparse store exposes a simple API to the main thread:

- Querying for the longest block of available contiguous data starting from a
    given file offset or ending at a given file offset.
- Indicating that a certain set of bytes, represented as a set of
    nonoverlapping segments of file offsets, are currently active, and that the
    background thread should load them as soon as possible and avoid unloading
    them.

The sparse store is implemented as an array of segments, each segment
representing a contiguous file range, and carries its data in RAM.
No two segments may "touch", that is, there is no pair of segments that start
and end at the same position.
When loading a range of data, segments may need to be merged.

Many operations require handling O(N) data.
These operations are done in the background thread, the with sparse store mutex
unlocked.

## The linemap tree

The linemap tree is a tree that allows mapping between spatial positions and
offsets within the "virtual file".
