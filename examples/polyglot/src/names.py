from unsworn import json


def initials(req, ctx):
    return json("".join(word[0].upper() for word in req.json["name"].split()))
