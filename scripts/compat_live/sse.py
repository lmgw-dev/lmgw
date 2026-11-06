"""Server-sent events: split a captured stream into (event, data) pairs."""


def parse(text):
    """Return [(event_name_or_None, data_str)] in order, per the SSE field rules.

    Comment lines (starting with ':') are ignored; several data lines join with a
    newline; an event without data lines is dropped, as EventSource drops it.
    """
    events = []
    name, data = None, []
    for raw in text.replace("\r\n", "\n").replace("\r", "\n").split("\n"):
        if raw == "":
            if data:
                events.append((name, "\n".join(data)))
            name, data = None, []
            continue
        if raw.startswith(":"):
            continue
        field, _, value = raw.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if field == "event":
            name = value
        elif field == "data":
            data.append(value)
    if data:
        events.append((name, "\n".join(data)))
    return events
