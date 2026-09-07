// A SECOND IMPLEMENTATION against forge.
//
// Stock git is the reference client, so a suite driven by it proves
// forge speaks what git speaks. It cannot prove forge speaks the
// PROTOCOL rather than git's habits: a server that depended on the
// exact order the reference client sends things would pass every leg
// of that suite and fail the first other client a user brings.
//
// go-git is an independent implementation of the smart HTTP protocol
// (Apache-2.0, github.com/go-git/go-git), written by nobody who has
// read forge. This clones through it, commits, pushes, and reads its
// own push back off the advertisement.
//
// The door's principal rides `X-Remote-User`, which go-git has no knob
// for, so the header goes on through a RoundTripper — the same place a
// gateway would put it.
//
//	go run . <url> <remote-user> <workdir>
package main

import (
	"fmt"
	"net/http"
	"os"
	"time"

	"github.com/go-git/go-git/v5"
	"github.com/go-git/go-git/v5/config"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
	"github.com/go-git/go-git/v5/plumbing/transport/client"
	githttp "github.com/go-git/go-git/v5/plumbing/transport/http"
)

type withUser struct {
	user string
	next http.RoundTripper
}

func (w withUser) RoundTrip(r *http.Request) (*http.Response, error) {
	r.Header.Set("X-Remote-User", w.user)
	return w.next.RoundTrip(r)
}

func die(what string, err error) {
	fmt.Printf("  FAIL  go-git: %s: %v\n", what, err)
	os.Exit(1)
}

func main() {
	if len(os.Args) != 4 {
		fmt.Println("usage: go run . <url> <remote-user> <workdir>")
		os.Exit(2)
	}
	url, user, dir := os.Args[1], os.Args[2], os.Args[3]
	client.InstallProtocol("http", githttp.NewClient(&http.Client{
		Transport: withUser{user: user, next: http.DefaultTransport},
	}))

	repo, err := git.PlainClone(dir, false, &git.CloneOptions{URL: url})
	if err != nil {
		die("clone", err)
	}
	head, err := repo.Head()
	if err != nil {
		die("head", err)
	}
	fmt.Printf("  PASS  go-git cloned over smart HTTP, HEAD = %s\n", head.Hash())

	wt, err := repo.Worktree()
	if err != nil {
		die("worktree", err)
	}
	name := fmt.Sprintf("gogit-%d.txt", time.Now().UnixNano())
	if err := os.WriteFile(dir+"/"+name, []byte("written by a second implementation\n"), 0o644); err != nil {
		die("write", err)
	}
	if _, err := wt.Add(name); err != nil {
		die("add", err)
	}
	commit, err := wt.Commit("a commit from a second implementation", &git.CommitOptions{
		Author: &object.Signature{Name: "go-git", Email: "gogit@invalid", When: time.Now()},
	})
	if err != nil {
		die("commit", err)
	}
	branch := plumbing.NewBranchReferenceName("gogit")
	if err := repo.Storer.SetReference(plumbing.NewHashReference(branch, commit)); err != nil {
		die("set ref", err)
	}
	if err := repo.Push(&git.PushOptions{
		RemoteName: "origin",
		RefSpecs:   []config.RefSpec{config.RefSpec(branch + ":" + branch)},
	}); err != nil {
		die("push", err)
	}
	fmt.Printf("  PASS  go-git pushed %s to refs/heads/gogit\n", commit)

	rem, err := repo.Remote("origin")
	if err != nil {
		die("remote", err)
	}
	refs, err := rem.List(&git.ListOptions{})
	if err != nil {
		die("ls-remote", err)
	}
	for _, r := range refs {
		if r.Name() == branch {
			if r.Hash() != commit {
				die("ls-remote", fmt.Errorf("the server advertises %s where we pushed %s", r.Hash(), commit))
			}
			fmt.Printf("  PASS  go-git read its own push back off the advertisement\n")
			return
		}
	}
	die("ls-remote", fmt.Errorf("the pushed branch is not advertised"))
}
