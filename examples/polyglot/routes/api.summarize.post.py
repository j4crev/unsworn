from unsworn import json
from unsworn_gen import ApiSummarizePostRequest


def handler(req: ApiSummarizePostRequest, ctx):
    words = req.json["text"].split()
    return json({"words": len(words), "summary": " ".join(words[:12]), "tenant": ctx.get("tenant")})
