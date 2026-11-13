from unsworn import json


def handler(req, ctx):
    words = req.json["text"].split()
    return json({"words": len(words), "summary": " ".join(words[:12]), "tenant": ctx.get("tenant")})
