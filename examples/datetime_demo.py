"""Dates and times (fixed values, so output is reproducible)."""

from datetime import date, datetime, timedelta

launch = date(2024, 3, 15)
print("launch:", launch.isoformat())
print("weekday:", launch.weekday(), "(Mon=0)")
print("formatted:", launch.strftime("%Y-%m-%d"))

# Arithmetic with timedeltas.
week_later = launch + timedelta(days=7)
print("a week later:", week_later.isoformat())

deadline = date(2024, 12, 31)
remaining = deadline - launch
print("days until deadline:", remaining.days)

# Datetimes carry a time component.
event = datetime(2024, 3, 15, 14, 30, 0)
print("event:", event.isoformat())
print("hour/minute:", event.hour, event.minute)

meeting = event + timedelta(hours=2, minutes=45)
print("meeting ends:", meeting.isoformat())

# Build a small schedule.
start = datetime(2024, 1, 1, 9, 0)
slots = [start + timedelta(minutes=30 * i) for i in range(4)]
print("slots:", [s.strftime("%H:%M") for s in slots])

# Compare and sort dates.
milestones = [date(2024, 6, 1), date(2024, 1, 15), date(2024, 3, 15)]
print("sorted:", [d.isoformat() for d in sorted(milestones)])
