// The handler bench/run.py's "work" group calls. See search.ts.
package main

import "github.com/j4crev/unsworn/sdk/go/unsworn"

var syllables = []string{
	"al", "be", "cor", "del", "en", "fa", "gra", "hy", "in", "jo", "ka", "lu", "mor",
	"ne", "or", "pra", "qui", "ro", "sta", "tel", "ur", "ve", "wil", "xa", "yo", "zen",
}

var names = makeNames()

func main() {
	unsworn.Serve(search)
}

func makeNames() []string {
	seed := 42
	next := func() int {
		seed = seed * 48271 % 2147483647
		return seed
	}
	names := make([]string, 0, 2000)
	for i := 0; i < 2000; i++ {
		name := ""
		for n := 3 + next()%4; n > 0; n-- {
			name += syllables[next()%len(syllables)]
		}
		names = append(names, name)
	}
	return names
}

// distance is the edit distance between two strings, keeping one row of the
// table.
func distance(a, b string, row []int) int {
	for j := 0; j <= len(b); j++ {
		row[j] = j
	}
	for i := 1; i <= len(a); i++ {
		diagonal := row[0]
		row[0] = i
		ca := a[i-1]
		for j := 1; j <= len(b); j++ {
			above := row[j]
			if ca == b[j-1] {
				row[j] = diagonal
			} else {
				row[j] = 1 + min(diagonal, above, row[j-1])
			}
			diagonal = above
		}
	}
	return row[len(b)]
}

type match struct {
	index, distance int
}

func search(req *unsworn.Request) (unsworn.Reply, error) {
	q := req.QueryValue("q")
	row := make([]int, len(q)+1)
	best := make([]match, 0, 6) // closest first
	for index, name := range names {
		d := distance(name, q, row)
		if len(best) == 5 && d >= best[4].distance {
			continue
		}
		at := len(best)
		for at > 0 && best[at-1].distance > d {
			at--
		}
		best = append(best, match{})
		copy(best[at+1:], best[at:])
		best[at] = match{index, d}
		if len(best) > 5 {
			best = best[:5]
		}
	}
	matches := make([]map[string]any, len(best))
	for i, m := range best {
		matches[i] = map[string]any{"name": names[m.index], "distance": m.distance}
	}
	return unsworn.JSON(map[string]any{"query": q, "matches": matches}), nil
}
