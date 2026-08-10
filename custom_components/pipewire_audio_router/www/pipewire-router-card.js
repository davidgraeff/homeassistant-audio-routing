var Hn = Array.isArray, Xr = Array.prototype.indexOf, Et = Array.prototype.includes, Rt = Array.from, Zr = Object.defineProperty, ft = Object.getOwnPropertyDescriptor, Jr = Object.getOwnPropertyDescriptors, Qr = Object.prototype, ei = Array.prototype, zn = Object.getPrototypeOf, kn = Object.isExtensible;
const ti = () => {
};
function ni(e) {
  for (var t = 0; t < e.length; t++)
    e[t]();
}
function jn() {
  var e, t, n = new Promise((r, i) => {
    e = r, t = i;
  });
  return { promise: n, resolve: e, reject: t };
}
const H = 2, We = 4, Ot = 8, qn = 1 << 24, re = 16, oe = 32, Ne = 64, Gt = 128, Q = 512, L = 1024, I = 2048, fe = 4096, j = 8192, W = 16384, et = 32768, Yt = 1 << 25, Ke = 65536, kt = 1 << 17, ri = 1 << 18, tt = 1 << 19, ii = 1 << 20, _e = 1 << 25, je = 65536, xt = 1 << 21, Ve = 1 << 22, Re = 1 << 23, yt = Symbol("$state"), si = Symbol(""), Bn = Symbol("attributes"), Ut = Symbol("class"), Vt = Symbol("style"), $t = Symbol("text"), Nt = new class extends Error {
  name = "StaleReactionError";
  message = "The reaction that called `getAbortSignal()` was re-run or destroyed";
}();
function oi() {
  throw new Error("https://svelte.dev/e/async_derived_orphan");
}
function li(e, t, n) {
  throw new Error("https://svelte.dev/e/each_key_duplicate");
}
function ai(e) {
  throw new Error("https://svelte.dev/e/effect_in_teardown");
}
function fi() {
  throw new Error("https://svelte.dev/e/effect_in_unowned_derived");
}
function ui(e) {
  throw new Error("https://svelte.dev/e/effect_orphan");
}
function ci() {
  throw new Error("https://svelte.dev/e/effect_update_depth_exceeded");
}
function hi() {
  throw new Error("https://svelte.dev/e/state_descriptors_fixed");
}
function di() {
  throw new Error("https://svelte.dev/e/state_prototype_fixed");
}
function vi() {
  throw new Error("https://svelte.dev/e/state_unsafe_mutation");
}
function pi() {
  throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror");
}
const _i = 1, gi = 2, mi = 16, yi = 1, wi = 2, O = Symbol("uninitialized"), bi = "http://www.w3.org/1999/xhtml";
function Ei() {
  console.warn("https://svelte.dev/e/derived_inert");
}
function ki() {
  console.warn("https://svelte.dev/e/svelte_boundary_reset_noop");
}
function Gn(e) {
  return e === this.v;
}
function xi(e, t) {
  return e != e ? t == t : e !== t || e !== null && typeof e == "object" || typeof e == "function";
}
function Yn(e) {
  return !xi(e, this.v);
}
let q = null;
function Xe(e) {
  q = e;
}
function tn(e, t = !1, n) {
  q = {
    p: q,
    i: !1,
    c: null,
    e: null,
    s: e,
    x: null,
    r: (
      /** @type {Effect} */
      E
    ),
    l: null
  };
}
function nn(e) {
  var t = (
    /** @type {ComponentContext} */
    q
  ), n = t.e;
  if (n !== null) {
    t.e = null;
    for (var r of n)
      fr(r);
  }
  return t.i = !0, q = t.p, /** @type {T} */
  {};
}
function Un() {
  return !0;
}
let Ye = [];
function Ti() {
  var e = Ye;
  Ye = [], ni(e);
}
function $e(e) {
  if (Ye.length === 0) {
    var t = Ye;
    queueMicrotask(() => {
      t === Ye && Ti();
    });
  }
  Ye.push(e);
}
function Vn(e) {
  var t = E;
  if (t === null)
    return k.f |= Re, e;
  if ((t.f & et) === 0 && (t.f & We) === 0)
    throw e;
  Se(e, t);
}
function Se(e, t) {
  if (!(t !== null && (t.f & W) !== 0)) {
    for (; t !== null; ) {
      if ((t.f & Gt) !== 0) {
        if ((t.f & et) === 0)
          throw e;
        try {
          t.b.error(e);
          return;
        } catch (n) {
          e = n;
        }
      }
      t = t.parent;
    }
    throw e;
  }
}
const Ai = -7169;
function M(e, t) {
  e.f = e.f & Ai | t;
}
function rn(e) {
  (e.f & Q) !== 0 || e.deps === null ? M(e, L) : M(e, fe);
}
function $n(e) {
  if (e !== null)
    for (const t of e)
      (t.f & H) === 0 || (t.f & je) === 0 || (t.f ^= je, $n(
        /** @type {Derived} */
        t.deps
      ));
}
function Wn(e, t, n) {
  (e.f & I) !== 0 ? t.add(e) : (e.f & fe) !== 0 && n.add(e), $n(e.deps), M(e, L);
}
function Si(e) {
  let t = 0, n = qe(0), r;
  return () => {
    an() && (u(n), cr(() => (t === 0 && (r = kr(() => e(() => ut(n)))), t += 1, () => {
      $e(() => {
        t -= 1, t === 0 && (r?.(), r = void 0, ut(n));
      });
    })));
  };
}
var Ci = Ke | tt;
function Mi(e, t, n, r) {
  new Ri(e, t, n, r);
}
class Ri {
  /** @type {Boundary | null} */
  parent;
  is_pending = !1;
  /**
   * API-level transformError transform function. Transforms errors before they reach the `failed` snippet.
   * Inherited from parent boundary, or defaults to identity.
   * @type {(error: unknown) => unknown}
   */
  transform_error;
  /** @type {TemplateNode} */
  #e;
  /** @type {TemplateNode | null} */
  #t = null;
  /** @type {BoundaryProps} */
  #n;
  /** @type {((anchor: Node) => void)} */
  #l;
  /** @type {Effect} */
  #s;
  /** @type {Effect | null} */
  #o = null;
  /** @type {Effect | null} */
  #r = null;
  /** @type {Effect | null} */
  #a = null;
  /** @type {DocumentFragment | null} */
  #i = null;
  #d = 0;
  #f = 0;
  #u = !1;
  /** @type {Set<Effect>} */
  #c = /* @__PURE__ */ new Set();
  /** @type {Set<Effect>} */
  #_ = /* @__PURE__ */ new Set();
  /**
   * A source containing the number of pending async deriveds/expressions.
   * Only created if `$effect.pending()` is used inside the boundary,
   * otherwise updating the source results in needless `Batch.ensure()`
   * calls followed by no-op flushes
   * @type {Source<number> | null}
   */
  #h = null;
  #m = Si(() => (this.#h = qe(this.#d), () => {
    this.#h = null;
  }));
  /**
   * @param {TemplateNode} node
   * @param {BoundaryProps} props
   * @param {((anchor: Node) => void)} children
   * @param {((error: unknown) => unknown) | undefined} [transform_error]
   */
  constructor(t, n, r, i) {
    this.#e = t, this.#n = n, this.#l = (s) => {
      var l = (
        /** @type {Effect} */
        E
      );
      l.b = this, l.f |= Gt, r(s);
    }, this.parent = /** @type {Effect} */
    E.b, this.transform_error = i ?? this.parent?.transform_error ?? ((s) => s), this.#s = fn(() => {
      this.#y();
    }, Ci);
  }
  #g() {
    try {
      this.#o = J(() => this.#l(this.#e));
    } catch (t) {
      this.error(t);
    }
  }
  /**
   * @param {unknown} error The deserialized error from the server's hydration comment
   */
  #b(t) {
    const n = this.#n.failed;
    n && (this.#a = J(() => {
      n(
        this.#e,
        () => t,
        () => () => {
        }
      );
    }));
  }
  #E() {
    const t = this.#n.pending;
    t && (this.is_pending = !0, this.#r = J(() => t(this.#e)), $e(() => {
      var n = this.#i = document.createDocumentFragment(), r = Oe();
      n.append(r), this.#o = this.#w(() => J(() => this.#l(r))), this.#f === 0 && (this.#e.before(n), this.#i = null, He(
        /** @type {Effect} */
        this.#r,
        () => {
          this.#r = null;
        }
      ), this.#v(
        /** @type {Batch} */
        x
      ));
    }));
  }
  #y() {
    try {
      if (this.is_pending = this.has_pending_snippet(), this.#f = 0, this.#d = 0, this.#o = J(() => {
        this.#l(this.#e);
      }), this.#f > 0) {
        var t = this.#i = document.createDocumentFragment();
        cn(this.#o, t);
        const n = (
          /** @type {(anchor: Node) => void} */
          this.#n.pending
        );
        this.#r = J(() => n(this.#e));
      } else
        this.#v(
          /** @type {Batch} */
          x
        );
    } catch (n) {
      this.error(n);
    }
  }
  /**
   * @param {Batch} batch
   */
  #v(t) {
    this.is_pending = !1, t.transfer_effects(this.#c, this.#_);
  }
  /**
   * Defer an effect inside a pending boundary until the boundary resolves
   * @param {Effect} effect
   */
  defer_effect(t) {
    Wn(t, this.#c, this.#_);
  }
  /**
   * Returns `false` if the effect exists inside a boundary whose pending snippet is shown
   * @returns {boolean}
   */
  is_rendered() {
    return !this.is_pending && (!this.parent || this.parent.is_rendered());
  }
  has_pending_snippet() {
    return !!this.#n.pending;
  }
  /**
   * @template T
   * @param {() => T} fn
   */
  #w(t) {
    var n = E, r = k, i = q;
    ue(this.#s), ee(this.#s), Xe(this.#s.ctx);
    try {
      return Le.ensure(), t();
    } catch (s) {
      return Vn(s), null;
    } finally {
      ue(n), ee(r), Xe(i);
    }
  }
  /**
   * Updates the pending count associated with the currently visible pending snippet,
   * if any, such that we can replace the snippet with content once work is done
   * @param {1 | -1} d
   * @param {Batch} batch
   */
  #p(t, n) {
    if (!this.has_pending_snippet()) {
      this.parent && this.parent.#p(t, n);
      return;
    }
    this.#f += t, this.#f === 0 && (this.#v(n), this.#r && He(this.#r, () => {
      this.#r = null;
    }), this.#i && (this.#e.before(this.#i), this.#i = null));
  }
  /**
   * Update the source that powers `$effect.pending()` inside this boundary,
   * and controls when the current `pending` snippet (if any) is removed.
   * Do not call from inside the class
   * @param {1 | -1} d
   * @param {Batch} batch
   */
  update_pending_count(t, n) {
    this.#p(t, n), this.#d += t, !(!this.#h || this.#u) && (this.#u = !0, $e(() => {
      this.#u = !1, this.#h && Ze(this.#h, this.#d);
    }));
  }
  get_effect_pending() {
    return this.#m(), u(
      /** @type {Source<number>} */
      this.#h
    );
  }
  /** @param {unknown} error */
  error(t) {
    if (!this.#n.onerror && !this.#n.failed)
      throw t;
    x?.is_fork ? (this.#o && x.skip_effect(this.#o), this.#r && x.skip_effect(this.#r), this.#a && x.skip_effect(this.#a), x.oncommit(() => {
      this.#k(t);
    })) : this.#k(t);
  }
  /**
   * @param {unknown} error
   */
  #k(t) {
    this.#o && (Y(this.#o), this.#o = null), this.#r && (Y(this.#r), this.#r = null), this.#a && (Y(this.#a), this.#a = null);
    var n = this.#n.onerror;
    let r = this.#n.failed;
    var i = !1, s = !1;
    const l = () => {
      if (i) {
        ki();
        return;
      }
      i = !0, s && pi(), this.#a !== null && He(this.#a, () => {
        this.#a = null;
      }), this.#w(() => {
        this.#y();
      });
    }, o = (a) => {
      try {
        s = !0, n?.(a, l), s = !1;
      } catch (f) {
        Se(f, this.#s && this.#s.parent);
      }
      r && (this.#a = this.#w(() => {
        try {
          return J(() => {
            var f = (
              /** @type {Effect} */
              E
            );
            f.b = this, f.f |= Gt, r(
              this.#e,
              () => a,
              () => l
            );
          });
        } catch (f) {
          return Se(
            f,
            /** @type {Effect} */
            this.#s.parent
          ), null;
        }
      }));
    };
    $e(() => {
      var a;
      try {
        a = this.transform_error(t);
      } catch (f) {
        Se(f, this.#s && this.#s.parent);
        return;
      }
      a !== null && typeof a == "object" && typeof /** @type {any} */
      a.then == "function" ? a.then(
        o,
        /** @param {unknown} e */
        (f) => Se(f, this.#s && this.#s.parent)
      ) : o(a);
    });
  }
}
function Oi(e, t, n, r) {
  const i = Lt;
  var s = e.filter((_) => !_.settled), l = t.map(i);
  if (n.length === 0 && s.length === 0) {
    r(l);
    return;
  }
  var o = (
    /** @type {Effect} */
    E
  ), a = Ni(), f = s.length === 1 ? s[0].promise : s.length > 1 ? Promise.all(s.map((_) => _.promise)) : null;
  function p(_) {
    if ((o.f & W) === 0) {
      a();
      try {
        r([...l, ..._]);
      } catch (h) {
        Se(h, o);
      }
      Tt();
    }
  }
  var d = Kn();
  if (n.length === 0) {
    f.then(() => p([])).finally(d);
    return;
  }
  function v() {
    Promise.all(n.map((_) => /* @__PURE__ */ Li(_))).then(p).catch((_) => Se(_, o)).finally(d);
  }
  f ? f.then(() => {
    a(), v(), Tt();
  }) : v();
}
function Ni() {
  var e = (
    /** @type {Effect} */
    E
  ), t = k, n = q, r = (
    /** @type {Batch} */
    x
  );
  return function(s = !0) {
    ue(e), ee(t), Xe(n), s && (e.f & W) === 0 && (r?.activate(), r?.apply());
  };
}
function Tt(e = !0) {
  ue(null), ee(null), Xe(null), e && x?.deactivate();
}
function Kn() {
  var e = (
    /** @type {Effect} */
    E
  ), t = e.b, n = (
    /** @type {Batch} */
    x
  ), r = !!t?.is_rendered();
  return t?.update_pending_count(1, n), n.increment(r, e), () => {
    t?.update_pending_count(-1, n), n.decrement(r, e);
  };
}
// @__NO_SIDE_EFFECTS__
function Lt(e) {
  var t = H | I;
  return E !== null && (E.f |= tt), {
    ctx: q,
    deps: null,
    effects: null,
    equals: Gn,
    f: t,
    fn: e,
    reactions: null,
    rv: 0,
    v: (
      /** @type {V} */
      O
    ),
    wv: 0,
    parent: E,
    ac: null
  };
}
const ot = Symbol("obsolete");
// @__NO_SIDE_EFFECTS__
function Li(e, t, n) {
  let r = (
    /** @type {Effect | null} */
    E
  );
  r === null && oi();
  var i = (
    /** @type {Promise<V>} */
    /** @type {unknown} */
    void 0
  ), s = qe(
    /** @type {V} */
    O
  ), l = !k, o = /* @__PURE__ */ new Set();
  return $i(() => {
    var a = (
      /** @type {Effect} */
      E
    ), f = jn();
    i = f.promise;
    try {
      Promise.resolve(e()).then(f.resolve, (_) => {
        _ !== Nt && f.reject(_);
      }).finally(Tt);
    } catch (_) {
      f.reject(_), Tt();
    }
    var p = (
      /** @type {Batch} */
      x
    );
    if (l) {
      if ((a.f & et) !== 0)
        var d = Kn();
      if (
        // boundary can be null if the async derived is inside an $effect.root not connected to the component render tree
        r.b?.is_rendered()
      )
        p.async_deriveds.get(a)?.reject(ot);
      else
        for (const _ of o.values())
          _.reject(ot);
      o.add(f), p.async_deriveds.set(a, f);
    }
    const v = (_, h = void 0) => {
      d?.(), o.delete(f), h !== ot && (p.activate(), h ? (s.f |= Re, Ze(s, h)) : ((s.f & Re) !== 0 && (s.f ^= Re), Ze(s, _)), p.deactivate());
    };
    f.promise.then(v, (_) => v(null, _ || "unknown"));
  }), Ui(() => {
    for (const a of o)
      a.reject(ot);
  }), new Promise((a) => {
    function f(p) {
      function d() {
        p === i ? a(s) : f(i);
      }
      p.then(d, d);
    }
    f(i);
  });
}
// @__NO_SIDE_EFFECTS__
function R(e) {
  const t = /* @__PURE__ */ Lt(e);
  return _r(t), t;
}
// @__NO_SIDE_EFFECTS__
function Di(e) {
  const t = /* @__PURE__ */ Lt(e);
  return t.equals = Yn, t;
}
function Pi(e) {
  var t = e.effects;
  if (t !== null) {
    e.effects = null;
    for (var n = 0; n < t.length; n += 1)
      Y(
        /** @type {Effect} */
        t[n]
      );
  }
}
function sn(e) {
  var t, n = E, r = e.parent;
  if (!ge && r !== null && e.v !== O && // if it was never evaluated before, it's guaranteed to fail downstream, so we try to execute instead
  (r.f & (W | j)) !== 0)
    return Ei(), e.v;
  ue(r);
  try {
    e.f &= ~je, Pi(e), t = wr(e);
  } finally {
    ue(n);
  }
  return t;
}
function Xn(e) {
  var t = sn(e);
  if (!e.equals(t) && (e.wv = mr(), (!x?.is_fork || e.deps === null) && (x !== null ? (x.capture(e, t, !0), Wt?.capture(e, t, !0)) : e.v = t, e.deps === null))) {
    M(e, L);
    return;
  }
  ge || (ie !== null ? (an() || x?.is_fork) && ie.set(e, t) : rn(e));
}
function Fi(e) {
  if (e.effects !== null)
    for (const t of e.effects)
      (t.teardown || t.ac) && (t.teardown?.(), t.ac?.abort(Nt), t.fn !== null && (t.teardown = ti), t.ac = null, ct(t, 0), un(t));
}
function Zn(e) {
  if (e.effects !== null)
    for (const t of e.effects)
      t.teardown && t.fn !== null && Je(t);
}
let Ht = null, Be = null, x = null, Wt = null, ie = null, Kt = null, zt = !1, Ue = null, wt = null;
var xn = 0;
let Ii = 1;
class Le {
  id = Ii++;
  /** True as soon as `#process` was called */
  #e = !1;
  linked = !0;
  /** @type {Batch | null} */
  #t = null;
  /** @type {Batch | null} */
  #n = null;
  /** @type {Map<Effect, ReturnType<typeof deferred<any>>>} */
  async_deriveds = /* @__PURE__ */ new Map();
  /**
   * The current values of any signals that are updated in this batch.
   * Tuple format: [value, is_derived] (note: is_derived is false for deriveds, too, if they were overridden via assignment)
   * They keys of this map are identical to `this.#previous`
   * @type {Map<Value, [any, boolean]>}
   */
  current = /* @__PURE__ */ new Map();
  /**
   * The values of any signals (sources and deriveds) that are updated in this batch _before_ those updates took place.
   * They keys of this map are identical to `this.#current`
   * @type {Map<Value, any>}
   */
  previous = /* @__PURE__ */ new Map();
  /**
   * When the batch is committed (and the DOM is updated), we need to remove old branches
   * and append new ones by calling the functions added inside (if/each/key/etc) blocks
   * @type {Set<(batch: Batch) => void>}
   */
  #l = /* @__PURE__ */ new Set();
  /**
   * If a fork is discarded, we need to destroy any effects that are no longer needed
   * @type {Set<(batch: Batch) => void>}
   */
  #s = /* @__PURE__ */ new Set();
  /**
   * The number of async effects that are currently in flight
   */
  #o = 0;
  /**
   * Async effects that are currently in flight, _not_ inside a pending boundary
   * @type {Map<Effect, number>}
   */
  #r = /* @__PURE__ */ new Map();
  /**
   * A deferred that resolves when the batch is committed, used with `settled()`
   * TODO replace with Promise.withResolvers once supported widely enough
   * @type {{ promise: Promise<void>, resolve: (value?: any) => void, reject: (reason: unknown) => void } | null}
   */
  #a = null;
  /**
   * The root effects that need to be flushed
   * @type {Effect[]}
   */
  #i = [];
  /**
   * Effects created while this batch was active.
   * @type {Effect[]}
   */
  #d = [];
  /**
   * Deferred effects (which run after async work has completed) that are DIRTY
   * @type {Set<Effect>}
   */
  #f = /* @__PURE__ */ new Set();
  /**
   * Deferred effects that are MAYBE_DIRTY
   * @type {Set<Effect>}
   */
  #u = /* @__PURE__ */ new Set();
  /**
   * A map of branches that still exist, but will be destroyed when this batch
   * is committed — we skip over these during `process`.
   * The value contains child effects that were dirty/maybe_dirty before being reset,
   * so they can be rescheduled if the branch survives.
   * @type {Map<Effect, { d: Effect[], m: Effect[] }>}
   */
  #c = /* @__PURE__ */ new Map();
  /**
   * Inverse of #skipped_branches which we need to tell prior batches to unskip them when committing
   * @type {Set<Effect>}
   */
  #_ = /* @__PURE__ */ new Set();
  is_fork = !1;
  #h = !1;
  constructor() {
    Be === null ? Ht = Be = this : (Be.#n = this, this.#t = Be), Be = this;
  }
  #m() {
    if (this.is_fork) return !0;
    for (const r of this.#r.keys()) {
      for (var t = r, n = !1; t.parent !== null; ) {
        if (this.#c.has(t)) {
          n = !0;
          break;
        }
        t = t.parent;
      }
      if (!n)
        return !0;
    }
    return !1;
  }
  /**
   * Add an effect to the #skipped_branches map and reset its children
   * @param {Effect} effect
   */
  skip_effect(t) {
    this.#c.has(t) || this.#c.set(t, { d: [], m: [] }), this.#_.delete(t);
  }
  /**
   * Remove an effect from the #skipped_branches map and reschedule
   * any tracked dirty/maybe_dirty child effects
   * @param {Effect} effect
   * @param {(e: Effect) => void} callback
   */
  unskip_effect(t, n = (r) => this.schedule(r)) {
    var r = this.#c.get(t);
    if (r) {
      this.#c.delete(t);
      for (var i of r.d)
        M(i, I), n(i);
      for (i of r.m)
        M(i, fe), n(i);
    }
    this.#_.add(t);
  }
  #g() {
    this.#e = !0, xn++ > 1e3 && (this.#p(), Hi());
    for (const a of this.#f)
      this.#u.delete(a), M(a, I), this.schedule(a);
    for (const a of this.#u)
      M(a, fe), this.schedule(a);
    const t = this.#i;
    this.#i = [], this.apply();
    var n = Ue = [], r = [], i = wt = [];
    for (const a of t)
      try {
        this.#b(a, n, r);
      } catch (f) {
        throw er(a), this.#m() || this.discard(), f;
      }
    if (x = null, i.length > 0) {
      var s = Le.ensure();
      for (const a of i)
        s.schedule(a);
    }
    if (Ue = null, wt = null, this.#m()) {
      this.#v(r), this.#v(n);
      for (const [a, f] of this.#c)
        Qn(a, f);
      i.length > 0 && /** @type {unknown} */
      x.#g();
      return;
    }
    const l = this.#E();
    if (l) {
      this.#v(r), this.#v(n), l.#y(this);
      return;
    }
    this.#f.clear(), this.#u.clear();
    for (const a of this.#l) a(this);
    this.#l.clear(), Wt = this, Tn(r), Tn(n), Wt = null, this.#a?.resolve();
    var o = (
      /** @type {Batch | null} */
      /** @type {unknown} */
      x
    );
    if (this.#o === 0 && (this.#i.length === 0 || o !== null) && this.#p(), this.#i.length > 0)
      if (o !== null) {
        const a = o;
        a.#i.push(...this.#i.filter((f) => !a.#i.includes(f)));
      } else
        o = this;
    o !== null && o.#g();
  }
  /**
   * Traverse the effect tree, executing effects or stashing
   * them for later execution as appropriate
   * @param {Effect} root
   * @param {Effect[]} effects
   * @param {Effect[]} render_effects
   */
  #b(t, n, r) {
    t.f ^= L;
    for (var i = t.first; i !== null; ) {
      var s = i.f, l = (s & (oe | Ne)) !== 0, o = l && (s & L) !== 0, a = o || (s & j) !== 0 || this.#c.has(i);
      if (!a && i.fn !== null) {
        l ? i.f ^= L : (s & We) !== 0 ? n.push(i) : dt(i) && ((s & re) !== 0 && this.#u.add(i), Je(i));
        var f = i.first;
        if (f !== null) {
          i = f;
          continue;
        }
      }
      for (; i !== null; ) {
        var p = i.next;
        if (p !== null) {
          i = p;
          break;
        }
        i = i.parent;
      }
    }
  }
  #E() {
    for (var t = this.#t; t !== null; ) {
      if (!t.is_fork) {
        for (const [n, [, r]] of this.current)
          if (t.current.has(n) && !r)
            return t;
      }
      t = t.#t;
    }
    return null;
  }
  /**
   * @param {Batch} batch
   */
  #y(t) {
    for (const [r, i] of t.current)
      !this.previous.has(r) && t.previous.has(r) && this.previous.set(r, t.previous.get(r)), this.current.set(r, i);
    for (const [r, i] of t.async_deriveds) {
      const s = this.async_deriveds.get(r);
      s && i.promise.then(s.resolve).catch(s.reject);
    }
    t.async_deriveds.clear(), this.transfer_effects(t.#f, t.#u);
    const n = (r) => {
      var i = r.reactions;
      if (i !== null)
        for (const o of i) {
          var s = o.f;
          if ((s & H) !== 0)
            n(
              /** @type {Derived} */
              o
            );
          else {
            var l = (
              /** @type {Effect} */
              o
            );
            s & (Ve | re) && !this.async_deriveds.has(l) && (this.#u.delete(l), M(l, I), this.schedule(l));
          }
        }
    };
    for (const r of this.current.keys())
      n(r);
    this.oncommit(() => t.discard()), t.#p(), x = this, this.#g();
  }
  /**
   * @param {Effect[]} effects
   */
  #v(t) {
    for (var n = 0; n < t.length; n += 1)
      Wn(t[n], this.#f, this.#u);
  }
  /**
   * Associate a change to a given source with the current
   * batch, noting its previous and current values
   * @param {Value} source
   * @param {any} value
   * @param {boolean} [is_derived]
   */
  capture(t, n, r = !1) {
    t.v !== O && !this.previous.has(t) && this.previous.set(t, t.v), (t.f & Re) === 0 && (this.current.set(t, [n, r]), ie?.set(t, n)), this.is_fork || (t.v = n);
  }
  activate() {
    x = this;
  }
  deactivate() {
    x = null, ie = null;
  }
  flush() {
    try {
      zt = !0, x = this, this.#g();
    } finally {
      xn = 0, Kt = null, Ue = null, wt = null, zt = !1, x = null, ie = null, Ie.clear();
    }
  }
  discard() {
    for (const t of this.#s) t(this);
    this.#s.clear();
    for (const t of this.async_deriveds.values())
      t.reject(ot);
    this.#p(), this.#a?.resolve();
  }
  /**
   * @param {Effect} effect
   */
  register_created_effect(t) {
    this.#d.push(t);
  }
  #w() {
    for (let d = Ht; d !== null; d = d.#n) {
      var t = d.id < this.id, n = [];
      for (const [v, [_, h]] of this.current) {
        if (d.current.has(v)) {
          var r = (
            /** @type {[any, boolean]} */
            d.current.get(v)[0]
          );
          if (t && _ !== r)
            d.current.set(v, [_, h]);
          else
            continue;
        }
        n.push(v);
      }
      if (t)
        for (const [v, _] of this.async_deriveds) {
          const h = d.async_deriveds.get(v);
          h && _.promise.then(h.resolve).catch(h.reject);
        }
      var i = [...d.current.keys()].filter(
        (v) => !/** @type {[any, boolean]} */
        d.current.get(v)[1]
      );
      if (!(!d.#e || i.length === 0)) {
        var s = i.filter((v) => !this.current.has(v));
        if (s.length === 0)
          t && d.discard();
        else if (n.length > 0) {
          if (t)
            for (const v of this.#_)
              d.unskip_effect(v, (_) => {
                (_.f & (re | Ve)) !== 0 ? d.schedule(_) : d.#v([_]);
              });
          d.activate();
          var l = /* @__PURE__ */ new Set(), o = /* @__PURE__ */ new Map();
          for (var a of n)
            Jn(a, s, l, o);
          o = /* @__PURE__ */ new Map();
          var f = [...d.current].filter(([v, _]) => {
            const h = this.current.get(v);
            return h ? h[0] !== _[0] || h[1] !== _[1] : !0;
          }).map(([v]) => v);
          if (f.length > 0)
            for (const v of this.#d)
              (v.f & (W | j | kt)) === 0 && on(v, f, o) && ((v.f & (Ve | re)) !== 0 ? (M(v, I), d.schedule(v)) : d.#f.add(v));
          if (d.#i.length > 0 && !d.#h) {
            d.apply();
            for (var p of d.#i)
              d.#b(p, [], []);
            d.#i = [];
          }
          d.deactivate();
        }
      }
    }
  }
  /**
   * @param {boolean} blocking
   * @param {Effect} effect
   */
  increment(t, n) {
    if (this.#o += 1, t) {
      let r = this.#r.get(n) ?? 0;
      this.#r.set(n, r + 1);
    }
  }
  /**
   * @param {boolean} blocking
   * @param {Effect} effect
   */
  decrement(t, n) {
    if (this.#o -= 1, t) {
      let r = this.#r.get(n) ?? 0;
      r === 1 ? this.#r.delete(n) : this.#r.set(n, r - 1);
    }
    this.#h || (this.#h = !0, $e(() => {
      this.#h = !1, this.linked && this.flush();
    }));
  }
  /**
   * @param {Set<Effect>} dirty_effects
   * @param {Set<Effect>} maybe_dirty_effects
   */
  transfer_effects(t, n) {
    for (const r of t)
      this.#f.add(r);
    for (const r of n)
      this.#u.add(r);
    t.clear(), n.clear();
  }
  /** @param {(batch: Batch) => void} fn */
  oncommit(t) {
    this.#l.add(t);
  }
  /** @param {(batch: Batch) => void} fn */
  ondiscard(t) {
    this.#s.add(t);
  }
  settled() {
    return (this.#a ??= jn()).promise;
  }
  static ensure() {
    if (x === null) {
      const t = x = new Le();
      zt || $e(() => {
        t.#e || t.flush();
      });
    }
    return x;
  }
  apply() {
    {
      ie = null;
      return;
    }
  }
  /**
   *
   * @param {Effect} effect
   */
  schedule(t) {
    if (Kt = t, t.b?.is_pending && (t.f & (We | Ot | qn)) !== 0 && (t.f & et) === 0) {
      t.b.defer_effect(t);
      return;
    }
    for (var n = t; n.parent !== null; ) {
      n = n.parent;
      var r = n.f;
      if (Ue !== null && n === E && (k === null || (k.f & H) === 0))
        return;
      if ((r & (Ne | oe)) !== 0) {
        if ((r & L) === 0)
          return;
        n.f ^= L;
      }
    }
    this.#i.push(n);
  }
  #p() {
    if (this.linked) {
      var t = this.#t, n = this.#n;
      t === null ? Ht = n : t.#n = n, n === null ? Be = t : n.#t = t, this.linked = !1;
    }
  }
}
function Hi() {
  try {
    ci();
  } catch (e) {
    Se(e, Kt);
  }
}
let pe = null;
function Tn(e) {
  var t = e.length;
  if (t !== 0) {
    for (var n = 0; n < t; ) {
      var r = e[n++];
      if ((r.f & (W | j)) === 0 && dt(r) && (pe = /* @__PURE__ */ new Set(), Je(r), r.deps === null && r.first === null && r.nodes === null && r.teardown === null && r.ac === null && dr(r), pe?.size > 0)) {
        Ie.clear();
        for (const i of pe) {
          if ((i.f & (W | j)) !== 0) continue;
          const s = [i];
          let l = i.parent;
          for (; l !== null; )
            pe.has(l) && (pe.delete(l), s.push(l)), l = l.parent;
          for (let o = s.length - 1; o >= 0; o--) {
            const a = s[o];
            (a.f & (W | j)) === 0 && Je(a);
          }
        }
        pe.clear();
      }
    }
    pe = null;
  }
}
function Jn(e, t, n, r) {
  if (!n.has(e) && (n.add(e), e.reactions !== null))
    for (const i of e.reactions) {
      const s = i.f;
      (s & H) !== 0 ? Jn(
        /** @type {Derived} */
        i,
        t,
        n,
        r
      ) : (s & (Ve | re)) !== 0 && (s & I) === 0 && on(i, t, r) && (M(i, I), ln(
        /** @type {Effect} */
        i
      ));
    }
}
function on(e, t, n) {
  const r = n.get(e);
  if (r !== void 0) return r;
  if (e.deps !== null)
    for (const i of e.deps) {
      if (Et.call(t, i))
        return !0;
      if ((i.f & H) !== 0 && on(
        /** @type {Derived} */
        i,
        t,
        n
      ))
        return n.set(
          /** @type {Derived} */
          i,
          !0
        ), !0;
    }
  return n.set(e, !1), !1;
}
function ln(e) {
  x.schedule(e);
}
function Qn(e, t) {
  if (!((e.f & oe) !== 0 && (e.f & L) !== 0)) {
    (e.f & I) !== 0 ? t.d.push(e) : (e.f & fe) !== 0 && t.m.push(e), M(e, L);
    for (var n = e.first; n !== null; )
      Qn(n, t), n = n.next;
  }
}
function er(e) {
  M(e, L);
  for (var t = e.first; t !== null; )
    er(t), t = t.next;
}
let At = /* @__PURE__ */ new Set();
const Ie = /* @__PURE__ */ new Map();
let tr = !1;
function qe(e, t) {
  var n = {
    f: 0,
    // TODO ideally we could skip this altogether, but it causes type errors
    v: e,
    reactions: null,
    equals: Gn,
    rv: 0,
    wv: 0
  };
  return n;
}
// @__NO_SIDE_EFFECTS__
function N(e, t) {
  const n = qe(e);
  return _r(n), n;
}
// @__NO_SIDE_EFFECTS__
function zi(e, t = !1, n = !0) {
  const r = qe(e);
  return t || (r.equals = Yn), r;
}
function T(e, t, n = !1) {
  k !== null && // since we are untracking the function inside `$inspect.with` we need to add this check
  // to ensure we error if state is set inside an inspect effect
  (!se || (k.f & kt) !== 0) && Un() && (k.f & (H | re | Ve | kt)) !== 0 && (ae === null || !ae.has(e)) && vi();
  let r = n ? Ce(t) : t;
  return Ze(e, r, wt);
}
function Ze(e, t, n = null) {
  if (!e.equals(t)) {
    Ie.set(e, ge ? t : e.v);
    var r = Le.ensure();
    if (r.capture(e, t), (e.f & H) !== 0) {
      const i = (
        /** @type {Derived} */
        e
      );
      (e.f & I) !== 0 && sn(i), ie === null && rn(i);
    }
    e.wv = mr(), nr(e, I, n), E !== null && (E.f & L) !== 0 && (E.f & (oe | Ne)) === 0 && (Z === null ? Xi([e]) : Z.push(e)), !r.is_fork && At.size > 0 && !tr && ji();
  }
  return t;
}
function ji() {
  tr = !1;
  for (const e of At) {
    (e.f & L) !== 0 && M(e, fe);
    let t;
    try {
      t = dt(e);
    } catch {
      t = !0;
    }
    t && Je(e);
  }
  At.clear();
}
function ut(e) {
  T(e, e.v + 1);
}
function nr(e, t, n) {
  var r = e.reactions;
  if (r !== null)
    for (var i = r.length, s = 0; s < i; s++) {
      var l = r[s], o = l.f, a = (o & I) === 0;
      if (a && M(l, t), (o & kt) !== 0)
        At.add(
          /** @type {Effect} */
          l
        );
      else if ((o & H) !== 0) {
        var f = (
          /** @type {Derived} */
          l
        );
        ie?.delete(f), (o & je) === 0 && (o & Q && (E === null || (E.f & xt) === 0) && (l.f |= je), nr(f, fe, n));
      } else if (a) {
        var p = (
          /** @type {Effect} */
          l
        );
        (o & re) !== 0 && pe !== null && pe.add(p), n !== null ? n.push(p) : ln(p);
      }
    }
}
function Ce(e) {
  if (typeof e != "object" || e === null || yt in e)
    return e;
  const t = zn(e);
  if (t !== Qr && t !== ei)
    return e;
  var n = /* @__PURE__ */ new Map(), r = Hn(e), i = /* @__PURE__ */ N(0), s = ze, l = (o) => {
    if (ze === s)
      return o();
    var a = k, f = ze;
    ee(null), Cn(s);
    var p = o();
    return ee(a), Cn(f), p;
  };
  return r && n.set("length", /* @__PURE__ */ N(
    /** @type {any[]} */
    e.length
  )), new Proxy(
    /** @type {any} */
    e,
    {
      defineProperty(o, a, f) {
        (!("value" in f) || f.configurable === !1 || f.enumerable === !1 || f.writable === !1) && hi();
        var p = n.get(a);
        return p === void 0 ? l(() => {
          var d = /* @__PURE__ */ N(f.value);
          return n.set(a, d), d;
        }) : T(p, f.value, !0), !0;
      },
      deleteProperty(o, a) {
        var f = n.get(a);
        if (f === void 0) {
          if (a in o) {
            const p = l(() => /* @__PURE__ */ N(O));
            n.set(a, p), ut(i);
          }
        } else
          T(f, O), ut(i);
        return !0;
      },
      get(o, a, f) {
        if (a === yt)
          return e;
        var p = n.get(a), d = a in o;
        if (p === void 0 && (!d || ft(o, a)?.writable) && (p = l(() => {
          var _ = Ce(d ? o[a] : O), h = /* @__PURE__ */ N(_);
          return h;
        }), n.set(a, p)), p !== void 0) {
          var v = u(p);
          return v === O ? void 0 : v;
        }
        return Reflect.get(o, a, f);
      },
      getOwnPropertyDescriptor(o, a) {
        var f = Reflect.getOwnPropertyDescriptor(o, a);
        if (f && "value" in f) {
          var p = n.get(a);
          p && (f.value = u(p));
        } else if (f === void 0) {
          var d = n.get(a), v = d?.v;
          if (d !== void 0 && v !== O)
            return {
              enumerable: !0,
              configurable: !0,
              value: v,
              writable: !0
            };
        }
        return f;
      },
      has(o, a) {
        if (a === yt)
          return !0;
        var f = n.get(a), p = f !== void 0 && f.v !== O || Reflect.has(o, a);
        if (f !== void 0 || E !== null && (!p || ft(o, a)?.writable)) {
          f === void 0 && (f = l(() => {
            var v = p ? Ce(o[a]) : O, _ = /* @__PURE__ */ N(v);
            return _;
          }), n.set(a, f));
          var d = u(f);
          if (d === O)
            return !1;
        }
        return p;
      },
      set(o, a, f, p) {
        var d = n.get(a), v = a in o;
        if (r && a === "length")
          for (var _ = f; _ < /** @type {Source<number>} */
          d.v; _ += 1) {
            var h = n.get(_ + "");
            h !== void 0 ? T(h, O) : _ in o && (h = l(() => /* @__PURE__ */ N(O)), n.set(_ + "", h));
          }
        if (d === void 0)
          (!v || ft(o, a)?.writable) && (d = l(() => /* @__PURE__ */ N(void 0)), T(d, Ce(f)), n.set(a, d));
        else {
          v = d.v !== O;
          var w = l(() => Ce(f));
          T(d, w);
        }
        var m = Reflect.getOwnPropertyDescriptor(o, a);
        if (m?.set && m.set.call(p, f), !v) {
          if (r && typeof a == "string") {
            var y = (
              /** @type {Source<number>} */
              n.get("length")
            ), b = Number(a);
            Number.isInteger(b) && b >= y.v && T(y, b + 1);
          }
          ut(i);
        }
        return !0;
      },
      ownKeys(o) {
        u(i);
        var a = Reflect.ownKeys(o).filter((d) => {
          var v = n.get(d);
          return v === void 0 || v.v !== O;
        });
        for (var [f, p] of n)
          p.v !== O && !(f in o) && a.push(f);
        return a;
      },
      setPrototypeOf() {
        di();
      }
    }
  );
}
var An, rr, ir, sr;
function qi() {
  if (An === void 0) {
    An = window, rr = /Firefox/.test(navigator.userAgent);
    var e = Element.prototype, t = Node.prototype, n = Text.prototype;
    ir = ft(t, "firstChild").get, sr = ft(t, "nextSibling").get, kn(e) && (e[Ut] = void 0, e[Bn] = null, e[Vt] = void 0, e.__e = void 0), kn(n) && (n[$t] = void 0);
  }
}
function Oe(e = "") {
  return document.createTextNode(e);
}
// @__NO_SIDE_EFFECTS__
function Me(e) {
  return (
    /** @type {TemplateNode | null} */
    ir.call(e)
  );
}
// @__NO_SIDE_EFFECTS__
function ht(e) {
  return (
    /** @type {TemplateNode | null} */
    sr.call(e)
  );
}
function ne(e, t) {
  return /* @__PURE__ */ Me(e);
}
function Xt(e, t = !1) {
  {
    var n = /* @__PURE__ */ Me(e);
    return n instanceof Comment && n.data === "" ? /* @__PURE__ */ ht(n) : n;
  }
}
function ve(e, t = 1, n = !1) {
  let r = e;
  for (; t--; )
    r = /** @type {TemplateNode} */
    /* @__PURE__ */ ht(r);
  return r;
}
function Bi(e) {
  e.textContent = "";
}
function or() {
  return !1;
}
function lr(e, t, n) {
  return (
    /** @type {T extends keyof HTMLElementTagNameMap ? HTMLElementTagNameMap[T] : Element} */
    n ? document.createElement(e, { is: n }) : document.createElement(e)
  );
}
function ar(e) {
  var t = k, n = E;
  ee(null), ue(null);
  try {
    return e();
  } finally {
    ee(t), ue(n);
  }
}
function Gi(e) {
  E === null && (k === null && ui(), fi()), ge && ai();
}
function Yi(e, t) {
  var n = t.last;
  n === null ? t.last = t.first = e : (n.next = e, e.prev = n, t.last = e);
}
function me(e, t) {
  var n = E;
  n !== null && (n.f & j) !== 0 && (e |= j);
  var r = {
    ctx: q,
    deps: null,
    nodes: null,
    f: e | I | Q,
    first: null,
    fn: t,
    last: null,
    next: null,
    parent: n,
    b: n && n.b,
    prev: null,
    teardown: null,
    wv: 0,
    ac: null
  };
  x?.register_created_effect(r);
  var i = r;
  if ((e & We) !== 0)
    Ue !== null ? Ue.push(r) : Le.ensure().schedule(r);
  else if (t !== null) {
    try {
      Je(r);
    } catch (l) {
      throw Y(r), l;
    }
    i.deps === null && i.teardown === null && i.nodes === null && i.first === i.last && // either `null`, or a singular child
    (i.f & tt) === 0 && (i = i.first, (e & re) !== 0 && (e & Ke) !== 0 && i !== null && (i.f |= Ke));
  }
  if (i !== null && (i.parent = n, n !== null && Yi(i, n), k !== null && (k.f & H) !== 0 && (e & Ne) === 0)) {
    var s = (
      /** @type {Derived} */
      k
    );
    (s.effects ??= []).push(i);
  }
  return r;
}
function an() {
  return k !== null && !se;
}
function Ui(e) {
  const t = me(Ot, null);
  return M(t, L), t.teardown = e, t;
}
function St(e) {
  Gi();
  var t = (
    /** @type {Effect} */
    E.f
  ), n = !k && (t & oe) !== 0 && q !== null && !q.i;
  if (n) {
    var r = (
      /** @type {ComponentContext} */
      q
    );
    (r.e ??= []).push(e);
  } else
    return fr(e);
}
function fr(e) {
  return me(We | ii, e);
}
function Vi(e) {
  Le.ensure();
  const t = me(Ne | tt, e);
  return (n = {}) => new Promise((r) => {
    n.outro ? He(t, () => {
      Y(t), r(void 0);
    }) : (Y(t), r(void 0));
  });
}
function ur(e) {
  return me(We, e);
}
function $i(e) {
  return me(Ve | tt, e);
}
function cr(e, t = 0) {
  return me(Ot | t, e);
}
function xe(e, t = [], n = [], r = []) {
  Oi(r, t, n, (i) => {
    me(Ot, () => {
      e(...i.map(u));
    });
  });
}
function fn(e, t = 0) {
  var n = me(re | t, e);
  return n;
}
function J(e) {
  return me(oe | tt, e);
}
function hr(e) {
  var t = e.teardown;
  if (t !== null) {
    const n = ge, r = k;
    Sn(!0), ee(null);
    try {
      t.call(null);
    } finally {
      Sn(n), ee(r);
    }
  }
}
function un(e, t = !1) {
  var n = e.first;
  for (e.first = e.last = null; n !== null; ) {
    const i = n.ac;
    i !== null && ar(() => {
      i.abort(Nt);
    });
    var r = n.next;
    (n.f & Ne) !== 0 ? n.parent = null : Y(n, t), n = r;
  }
}
function Wi(e) {
  for (var t = e.first; t !== null; ) {
    var n = t.next;
    (t.f & oe) === 0 && Y(t), t = n;
  }
}
function Y(e, t = !0) {
  var n = !1;
  (t || (e.f & ri) !== 0) && e.nodes !== null && e.nodes.end !== null && (Ki(
    e.nodes.start,
    /** @type {TemplateNode} */
    e.nodes.end
  ), n = !0), e.f |= Yt, un(e, t && !n), ct(e, 0);
  var r = e.nodes && e.nodes.t;
  if (r !== null)
    for (const s of r)
      s.stop();
  hr(e), e.f ^= Yt, e.f |= W;
  var i = e.parent;
  i !== null && i.first !== null && dr(e), e.next = e.prev = e.teardown = e.ctx = e.deps = e.fn = e.nodes = e.ac = e.b = null;
}
function Ki(e, t) {
  for (; e !== null; ) {
    var n = e === t ? null : /* @__PURE__ */ ht(e);
    e.remove(), e = n;
  }
}
function dr(e) {
  var t = e.parent, n = e.prev, r = e.next;
  n !== null && (n.next = r), r !== null && (r.prev = n), t !== null && (t.first === e && (t.first = r), t.last === e && (t.last = n));
}
function He(e, t, n = !0) {
  var r = [];
  vr(e, r, !0);
  var i = () => {
    n && Y(e), t && t();
  }, s = r.length;
  if (s > 0) {
    var l = () => --s || i();
    for (var o of r)
      o.out(l);
  } else
    i();
}
function vr(e, t, n) {
  if ((e.f & j) === 0) {
    e.f ^= j;
    var r = e.nodes && e.nodes.t;
    if (r !== null)
      for (const o of r)
        (o.is_global || n) && t.push(o);
    for (var i = e.first; i !== null; ) {
      var s = i.next;
      if ((i.f & Ne) === 0) {
        var l = (i.f & Ke) !== 0 || // If this is a branch effect without a block effect parent,
        // it means the parent block effect was pruned. In that case,
        // transparency information was transferred to the branch effect.
        (i.f & oe) !== 0 && (e.f & re) !== 0;
        vr(i, t, l ? n : !1);
      }
      i = s;
    }
  }
}
function Ct(e) {
  pr(e, !0);
}
function pr(e, t) {
  if ((e.f & j) !== 0) {
    e.f ^= j, (e.f & L) === 0 && (M(e, I), Le.ensure().schedule(e));
    for (var n = e.first; n !== null; ) {
      var r = n.next, i = (n.f & Ke) !== 0 || (n.f & oe) !== 0;
      pr(n, i ? t : !1), n = r;
    }
    var s = e.nodes && e.nodes.t;
    if (s !== null)
      for (const l of s)
        (l.is_global || t) && l.in();
  }
}
function cn(e, t) {
  if (e.nodes)
    for (var n = e.nodes.start, r = e.nodes.end; n !== null; ) {
      var i = n === r ? null : /* @__PURE__ */ ht(n);
      t.append(n), n = i;
    }
}
let bt = !1, ge = !1;
function Sn(e) {
  ge = e;
}
let k = null, se = !1;
function ee(e) {
  k = e;
}
let E = null;
function ue(e) {
  E = e;
}
let ae = null;
function _r(e) {
  k !== null && (ae ??= /* @__PURE__ */ new Set()).add(e);
}
let G = null, $ = 0, Z = null;
function Xi(e) {
  Z = e;
}
let gr = 1, Fe = 0, ze = Fe;
function Cn(e) {
  ze = e;
}
function mr() {
  return ++gr;
}
function dt(e) {
  var t = e.f;
  if ((t & I) !== 0)
    return !0;
  if (t & H && (e.f &= ~je), (t & fe) !== 0) {
    for (var n = (
      /** @type {Value[]} */
      e.deps
    ), r = n.length, i = 0; i < r; i++) {
      var s = n[i];
      if (dt(
        /** @type {Derived} */
        s
      ) && Xn(
        /** @type {Derived} */
        s
      ), s.wv > e.wv)
        return !0;
    }
    (t & Q) !== 0 && // During time traveling we don't want to reset the status so that
    // traversal of the graph in the other batches still happens
    ie === null && M(e, L);
  }
  return !1;
}
function yr(e, t, n = !0) {
  var r = e.reactions;
  if (r !== null && !(ae !== null && ae.has(e)))
    for (var i = 0; i < r.length; i++) {
      var s = r[i];
      (s.f & H) !== 0 ? yr(
        /** @type {Derived} */
        s,
        t,
        !1
      ) : t === s && (n ? M(s, I) : (s.f & L) !== 0 && M(s, fe), ln(
        /** @type {Effect} */
        s
      ));
    }
}
function wr(e) {
  var t = G, n = $, r = Z, i = k, s = ae, l = q, o = se, a = ze, f = e.f;
  G = /** @type {null | Value[]} */
  null, $ = 0, Z = null, k = (f & (oe | Ne)) === 0 ? e : null, ae = null, Xe(e.ctx), se = !1, ze = ++Fe, e.ac !== null && (ar(() => {
    e.ac.abort(Nt);
  }), e.ac = null);
  try {
    e.f |= xt;
    var p = (
      /** @type {Function} */
      e.fn
    ), d = p();
    e.f |= et;
    var v = e.deps, _ = x?.is_fork;
    if (G !== null) {
      var h;
      if (_ || ct(e, $), v !== null && $ > 0)
        for (v.length = $ + G.length, h = 0; h < G.length; h++)
          v[$ + h] = G[h];
      else
        e.deps = v = G;
      if (an() && (e.f & Q) !== 0)
        for (h = $; h < v.length; h++)
          (v[h].reactions ??= []).push(e);
    } else !_ && v !== null && $ < v.length && (ct(e, $), v.length = $);
    if (Un() && Z !== null && !se && v !== null && (e.f & (H | fe | I)) === 0)
      for (h = 0; h < /** @type {Source[]} */
      Z.length; h++)
        yr(
          Z[h],
          /** @type {Effect} */
          e
        );
    if (i !== null && i !== e) {
      if (Fe++, i.deps !== null)
        for (let w = 0; w < n; w += 1)
          i.deps[w].rv = Fe;
      if (t !== null)
        for (const w of t)
          w.rv = Fe;
      Z !== null && (r === null ? r = Z : r.push(.../** @type {Source[]} */
      Z));
    }
    return (e.f & Re) !== 0 && (e.f ^= Re), d;
  } catch (w) {
    return Vn(w);
  } finally {
    e.f ^= xt, G = t, $ = n, Z = r, k = i, ae = s, Xe(l), se = o, ze = a;
  }
}
function Zi(e, t) {
  let n = t.reactions;
  if (n !== null) {
    var r = Xr.call(n, e);
    if (r !== -1) {
      var i = n.length - 1;
      i === 0 ? n = t.reactions = null : (n[r] = n[i], n.pop());
    }
  }
  if (n === null && (t.f & H) !== 0 && // Destroying a child effect while updating a parent effect can cause a dependency to appear
  // to be unused, when in fact it is used by the currently-updating parent. Checking `new_deps`
  // allows us to skip the expensive work of disconnecting and immediately reconnecting it
  (G === null || !Et.call(G, t))) {
    var s = (
      /** @type {Derived} */
      t
    );
    (s.f & Q) !== 0 && (s.f ^= Q, s.f &= ~je), s.v !== O && rn(s), Fi(s), ct(s, 0);
  }
}
function ct(e, t) {
  var n = e.deps;
  if (n !== null)
    for (var r = t; r < n.length; r++)
      Zi(e, n[r]);
}
function Je(e) {
  var t = e.f;
  if ((t & W) === 0) {
    M(e, L);
    var n = E, r = bt;
    E = e, bt = !0;
    try {
      (t & (re | qn)) !== 0 ? Wi(e) : un(e), hr(e);
      var i = wr(e);
      e.teardown = typeof i == "function" ? i : null, e.wv = gr;
      var s;
    } finally {
      bt = r, E = n;
    }
  }
}
function u(e) {
  var t = e.f, n = (t & H) !== 0;
  if (k !== null && !se) {
    var r = E !== null && (E.f & W) !== 0;
    if (!r && (ae === null || !ae.has(e))) {
      var i = k.deps;
      if ((k.f & xt) !== 0)
        e.rv < Fe && (e.rv = Fe, G === null && i !== null && i[$] === e ? $++ : G === null ? G = [e] : G.push(e));
      else {
        k.deps ??= [], Et.call(k.deps, e) || k.deps.push(e);
        var s = e.reactions;
        s === null ? e.reactions = [k] : Et.call(s, k) || s.push(k);
      }
    }
  }
  if (ge && Ie.has(e))
    return Ie.get(e);
  if (n) {
    var l = (
      /** @type {Derived} */
      e
    );
    if (ge) {
      var o = l.v;
      return ((l.f & L) === 0 && l.reactions !== null || Er(l)) && (o = sn(l)), Ie.set(l, o), o;
    }
    var a = (l.f & Q) === 0 && !se && k !== null && (bt || (k.f & Q) !== 0), f = (l.f & et) === 0;
    dt(l) && (a && (l.f |= Q), Xn(l)), a && !f && (Zn(l), br(l));
  }
  if (ie?.has(e))
    return ie.get(e);
  if ((e.f & Re) !== 0)
    throw e.v;
  return e.v;
}
function br(e) {
  if (e.f |= Q, e.deps !== null)
    for (const t of e.deps)
      (t.reactions ??= []).push(e), (t.f & H) !== 0 && (t.f & Q) === 0 && (Zn(
        /** @type {Derived} */
        t
      ), br(
        /** @type {Derived} */
        t
      ));
}
function Er(e) {
  if (e.v === O) return !0;
  if (e.deps === null) return !1;
  for (const t of e.deps)
    if (Ie.has(t) || (t.f & H) !== 0 && Er(
      /** @type {Derived} */
      t
    ))
      return !0;
  return !1;
}
function kr(e) {
  var t = se;
  try {
    return se = !0, e();
  } finally {
    se = t;
  }
}
const Ji = ["touchstart", "touchmove"];
function Qi(e) {
  return Ji.includes(e);
}
const lt = Symbol("events"), xr = /* @__PURE__ */ new Set(), Zt = /* @__PURE__ */ new Set();
function _t(e, t, n) {
  (t[lt] ??= {})[e] = n;
}
function es(e) {
  for (var t = 0; t < e.length; t++)
    xr.add(e[t]);
  for (var n of Zt)
    n(e);
}
let Mn = null;
function Rn(e) {
  var t = this, n = (
    /** @type {Node} */
    t.ownerDocument
  ), r = e.type, i = e.composedPath?.() || [], s = (
    /** @type {null | Element} */
    i[0] || e.target
  );
  Mn = e;
  var l = 0, o = Mn === e && e[lt];
  if (o) {
    var a = i.indexOf(o);
    if (a !== -1 && (t === document || t === /** @type {any} */
    window)) {
      e[lt] = t;
      return;
    }
    var f = i.indexOf(t);
    if (f === -1)
      return;
    a <= f && (l = a);
  }
  if (s = /** @type {Element} */
  i[l] || e.target, s !== t) {
    Zr(e, "currentTarget", {
      configurable: !0,
      get() {
        return s || n;
      }
    });
    var p = k, d = E;
    ee(null), ue(null);
    try {
      for (var v, _ = []; s !== null && s !== t; ) {
        try {
          var h = s[lt]?.[r];
          h != null && (!/** @type {any} */
          s.disabled || // DOM could've been updated already by the time this is reached, so we check this as well
          // -> the target could not have been disabled because it emits the event in the first place
          e.target === s) && h.call(s, e);
        } catch (w) {
          v ? _.push(w) : v = w;
        }
        if (e.cancelBubble) break;
        l++, s = l < i.length ? (
          /** @type {Element} */
          i[l]
        ) : null;
      }
      if (v) {
        for (let w of _)
          queueMicrotask(() => {
            throw w;
          });
        throw v;
      }
    } finally {
      e[lt] = t, delete e.currentTarget, ee(p), ue(d);
    }
  }
}
const ts = (
  // We gotta write it like this because after downleveling the pure comment may end up in the wrong location
  globalThis?.window?.trustedTypes && /* @__PURE__ */ globalThis.window.trustedTypes.createPolicy("svelte-trusted-html", {
    /** @param {string} html */
    createHTML: (e) => e
  })
);
function ns(e) {
  return (
    /** @type {string} */
    ts?.createHTML(e) ?? e
  );
}
function Tr(e) {
  var t = lr("template");
  return t.innerHTML = ns(e.replaceAll("<!>", "<!---->")), t.content;
}
function Mt(e, t) {
  var n = (
    /** @type {Effect} */
    E
  );
  n.nodes === null && (n.nodes = { start: e, end: t, a: null, t: null });
}
// @__NO_SIDE_EFFECTS__
function K(e, t) {
  var n = (t & yi) !== 0, r = (t & wi) !== 0, i, s = !e.startsWith("<!>");
  return () => {
    i === void 0 && (i = Tr(s ? e : "<!>" + e), n || (i = /** @type {TemplateNode} */
    /* @__PURE__ */ Me(i)));
    var l = (
      /** @type {TemplateNode} */
      r || rr ? document.importNode(i, !0) : i.cloneNode(!0)
    );
    if (n) {
      var o = (
        /** @type {TemplateNode} */
        /* @__PURE__ */ Me(l)
      ), a = (
        /** @type {TemplateNode} */
        l.lastChild
      );
      Mt(o, a);
    } else
      Mt(l, l);
    return l;
  };
}
// @__NO_SIDE_EFFECTS__
function rs(e, t, n = "svg") {
  var r = !e.startsWith("<!>"), i = `<${n}>${r ? e : "<!>" + e}</${n}>`, s;
  return () => {
    if (!s) {
      var l = (
        /** @type {DocumentFragment} */
        Tr(i)
      ), o = (
        /** @type {Element} */
        /* @__PURE__ */ Me(l)
      );
      for (s = document.createDocumentFragment(); /* @__PURE__ */ Me(o); )
        s.appendChild(
          /** @type {TemplateNode} */
          /* @__PURE__ */ Me(o)
        );
    }
    var a = (
      /** @type {TemplateNode} */
      s.cloneNode(!0)
    );
    {
      var f = (
        /** @type {TemplateNode} */
        /* @__PURE__ */ Me(a)
      ), p = (
        /** @type {TemplateNode} */
        a.lastChild
      );
      Mt(f, p);
    }
    return a;
  };
}
// @__NO_SIDE_EFFECTS__
function is(e, t) {
  return /* @__PURE__ */ rs(e, t, "svg");
}
function ss() {
  var e = document.createDocumentFragment(), t = document.createComment(""), n = Oe();
  return e.append(t, n), Mt(t, n), e;
}
function z(e, t) {
  e !== null && e.before(
    /** @type {Node} */
    t
  );
}
function Ge(e, t) {
  var n = t == null ? "" : typeof t == "object" ? `${t}` : t;
  n !== /** @type {any} */
  (e[$t] ??= e.nodeValue) && (e[$t] = n, e.nodeValue = `${n}`);
}
function Ar(e, t) {
  return os(e, t);
}
const gt = /* @__PURE__ */ new Map();
function os(e, { target: t, anchor: n, props: r = {}, events: i, context: s, intro: l = !0, transformError: o }) {
  qi();
  var a = void 0, f = Vi(() => {
    var p = n ?? t.appendChild(Oe());
    Mi(
      /** @type {TemplateNode} */
      p,
      {
        pending: () => {
        }
      },
      (_) => {
        tn({});
        var h = (
          /** @type {ComponentContext} */
          q
        );
        s && (h.c = s), i && (r.$$events = i), a = e(_, r) || {}, nn();
      },
      o
    );
    var d = /* @__PURE__ */ new Set(), v = (_) => {
      for (var h = 0; h < _.length; h++) {
        var w = _[h];
        if (!d.has(w)) {
          d.add(w);
          var m = Qi(w);
          for (const D of [t, document]) {
            var y = gt.get(D);
            y === void 0 && (y = /* @__PURE__ */ new Map(), gt.set(D, y));
            var b = y.get(w);
            b === void 0 ? (D.addEventListener(w, Rn, { passive: m }), y.set(w, 1)) : y.set(w, b + 1);
          }
        }
      }
    };
    return v(Rt(xr)), Zt.add(v), () => {
      for (var _ of d)
        for (const m of [t, document]) {
          var h = (
            /** @type {Map<string, number>} */
            gt.get(m)
          ), w = (
            /** @type {number} */
            h.get(_)
          );
          --w == 0 ? (m.removeEventListener(_, Rn), h.delete(_), h.size === 0 && gt.delete(m)) : h.set(_, w);
        }
      Zt.delete(v), p !== n && p.parentNode?.removeChild(p);
    };
  });
  return Jt.set(a, f), a;
}
let Jt = /* @__PURE__ */ new WeakMap();
function Sr(e, t) {
  const n = Jt.get(e);
  return n ? (Jt.delete(e), n(t)) : Promise.resolve();
}
class ls {
  /** @type {TemplateNode} */
  anchor;
  /** @type {Map<Batch, Key>} */
  #e = /* @__PURE__ */ new Map();
  /**
   * Map of keys to effects that are currently rendered in the DOM.
   * These effects are visible and actively part of the document tree.
   * Example:
   * ```
   * {#if condition}
   * 	foo
   * {:else}
   * 	bar
   * {/if}
   * ```
   * Can result in the entries `true->Effect` and `false->Effect`
   * @type {Map<Key, Effect>}
   */
  #t = /* @__PURE__ */ new Map();
  /**
   * Similar to #onscreen with respect to the keys, but contains branches that are not yet
   * in the DOM, because their insertion is deferred.
   * @type {Map<Key, Branch>}
   */
  #n = /* @__PURE__ */ new Map();
  /**
   * Keys of effects that are currently outroing
   * @type {Set<Key>}
   */
  #l = /* @__PURE__ */ new Set();
  /**
   * Whether to pause (i.e. outro) on change, or destroy immediately.
   * This is necessary for `<svelte:element>`
   */
  #s = !0;
  /**
   * @param {TemplateNode} anchor
   * @param {boolean} transition
   */
  constructor(t, n = !0) {
    this.anchor = t, this.#s = n;
  }
  /**
   * @param {Batch} batch
   */
  #o = (t) => {
    if (this.#e.has(t)) {
      var n = (
        /** @type {Key} */
        this.#e.get(t)
      ), r = this.#t.get(n);
      if (r)
        Ct(r), this.#l.delete(n);
      else {
        var i = this.#n.get(n);
        i && (Ct(i.effect), this.#t.set(n, i.effect), this.#n.delete(n), i.fragment.lastChild.remove(), this.anchor.before(i.fragment), r = i.effect);
      }
      for (const [s, l] of this.#e) {
        if (this.#e.delete(s), s === t)
          break;
        const o = this.#n.get(l);
        o && (Y(o.effect), this.#n.delete(l));
      }
      for (const [s, l] of this.#t) {
        if (s === n || this.#l.has(s)) continue;
        const o = () => {
          if (Array.from(this.#e.values()).includes(s)) {
            var f = document.createDocumentFragment();
            cn(l, f), f.append(Oe()), this.#n.set(s, { effect: l, fragment: f });
          } else
            Y(l);
          this.#l.delete(s), this.#t.delete(s);
        };
        this.#s || !r ? (this.#l.add(s), He(l, o, !1)) : o();
      }
    }
  };
  /**
   * @param {Batch} batch
   */
  #r = (t) => {
    this.#e.delete(t);
    const n = Array.from(this.#e.values());
    for (const [r, i] of this.#n)
      n.includes(r) || (Y(i.effect), this.#n.delete(r));
  };
  /**
   *
   * @param {any} key
   * @param {null | ((target: TemplateNode) => void)} fn
   */
  ensure(t, n) {
    var r = (
      /** @type {Batch} */
      x
    ), i = or();
    if (n && !this.#t.has(t) && !this.#n.has(t))
      if (i) {
        var s = document.createDocumentFragment(), l = Oe();
        s.append(l), this.#n.set(t, {
          effect: J(() => n(l)),
          fragment: s
        });
      } else
        this.#t.set(
          t,
          J(() => n(this.anchor))
        );
    if (this.#e.set(r, t), i) {
      for (const [o, a] of this.#t)
        o === t ? r.unskip_effect(a) : r.skip_effect(a);
      for (const [o, a] of this.#n)
        o === t ? r.unskip_effect(a.effect) : r.skip_effect(a.effect);
      r.oncommit(this.#o), r.ondiscard(this.#r);
    } else
      this.#o(r);
  }
}
function Pe(e, t, n = !1) {
  var r = new ls(e), i = n ? Ke : 0;
  function s(l, o) {
    r.ensure(l, o);
  }
  fn(() => {
    var l = !1;
    t((o, a = 0) => {
      l = !0, s(a, o);
    }), l || s(-1, null);
  }, i);
}
function as(e, t, n) {
  for (var r = [], i = t.length, s, l = t.length, o = 0; o < i; o++) {
    let d = t[o];
    He(
      d,
      () => {
        if (s) {
          if (s.pending.delete(d), s.done.add(d), s.pending.size === 0) {
            var v = (
              /** @type {Set<EachOutroGroup>} */
              e.outrogroups
            );
            Qt(e, Rt(s.done)), v.delete(s), v.size === 0 && (e.outrogroups = null);
          }
        } else
          l -= 1;
      },
      !1
    );
  }
  if (l === 0) {
    var a = r.length === 0 && n !== null;
    if (a) {
      var f = (
        /** @type {Element} */
        n
      ), p = (
        /** @type {Element} */
        f.parentNode
      );
      Bi(p), p.append(f), e.items.clear();
    }
    Qt(e, t, !a);
  } else
    s = {
      pending: new Set(t),
      done: /* @__PURE__ */ new Set()
    }, (e.outrogroups ??= /* @__PURE__ */ new Set()).add(s);
}
function Qt(e, t, n = !0) {
  var r;
  if (e.pending.size > 0) {
    r = /* @__PURE__ */ new Set();
    for (const l of e.pending.values())
      for (const o of l)
        r.add(
          /** @type {EachItem} */
          e.items.get(o).e
        );
  }
  for (var i = 0; i < t.length; i++) {
    var s = t[i];
    if (r?.has(s)) {
      s.f |= _e;
      const l = document.createDocumentFragment();
      cn(s, l);
    } else
      Y(t[i], n);
  }
}
var On;
function jt(e, t, n, r, i, s = null) {
  var l = e, o = /* @__PURE__ */ new Map();
  {
    var a = (
      /** @type {Element} */
      e
    );
    l = a.appendChild(Oe());
  }
  var f = null, p = /* @__PURE__ */ Di(() => {
    var b = n();
    return (
      /** @type {V[]} */
      Hn(b) ? b : b == null ? [] : Rt(b)
    );
  }), d, v = /* @__PURE__ */ new Map(), _ = !0;
  function h(b) {
    (y.effect.f & W) === 0 && (y.pending.delete(b), y.fallback = f, fs(y, d, l, t, r), f !== null && (d.length === 0 ? (f.f & _e) === 0 ? Ct(f) : (f.f ^= _e, at(f, null, l)) : He(f, () => {
      f = null;
    })));
  }
  function w(b) {
    y.pending.delete(b);
  }
  var m = fn(() => {
    d = /** @type {V[]} */
    u(p);
    for (var b = d.length, D = /* @__PURE__ */ new Set(), U = (
      /** @type {Batch} */
      x
    ), V = or(), le = 0; le < b; le += 1) {
      var ye = d[le], B = r(ye, le), X = _ ? null : o.get(B);
      X ? (X.v && Ze(X.v, ye), X.i && Ze(X.i, le), V && U.unskip_effect(X.e)) : (X = us(
        o,
        _ ? l : On ??= Oe(),
        ye,
        B,
        le,
        i,
        t,
        n
      ), _ || (X.e.f |= _e), o.set(B, X)), D.add(B);
    }
    if (b === 0 && s && !f && (_ ? f = J(() => s(l)) : (f = J(() => s(On ??= Oe())), f.f |= _e)), b > D.size && li(), !_)
      if (v.set(U, D), V) {
        for (const [Dt, Pt] of o)
          D.has(Dt) || U.skip_effect(Pt.e);
        U.oncommit(h), U.ondiscard(w);
      } else
        h(U);
    u(p);
  }), y = { effect: m, items: o, pending: v, outrogroups: null, fallback: f };
  _ = !1;
}
function it(e) {
  for (; e !== null && (e.f & oe) === 0; )
    e = e.next;
  return e;
}
function fs(e, t, n, r, i) {
  var s = t.length, l = e.items, o = it(e.effect.first), a, f = null, p = [], d = [], v, _, h, w;
  for (w = 0; w < s; w += 1) {
    if (v = t[w], _ = i(v, w), h = /** @type {EachItem} */
    l.get(_).e, e.outrogroups !== null)
      for (const B of e.outrogroups)
        B.pending.delete(h), B.done.delete(h);
    if ((h.f & j) !== 0 && Ct(h), (h.f & _e) !== 0)
      if (h.f ^= _e, h === o)
        at(h, null, n);
      else {
        var m = f ? f.next : o;
        h === e.effect.last && (e.effect.last = h.prev), h.prev && (h.prev.next = h.next), h.next && (h.next.prev = h.prev), Te(e, f, h), Te(e, h, m), at(h, m, n), f = h, p = [], d = [], o = it(f.next);
        continue;
      }
    if (h !== o) {
      if (a !== void 0 && a.has(h)) {
        if (p.length < d.length) {
          var y = d[0], b;
          f = y.prev;
          var D = p[0], U = p[p.length - 1];
          for (b = 0; b < p.length; b += 1)
            at(p[b], y, n);
          for (b = 0; b < d.length; b += 1)
            a.delete(d[b]);
          Te(e, D.prev, U.next), Te(e, f, D), Te(e, U, y), o = y, f = U, w -= 1, p = [], d = [];
        } else
          a.delete(h), at(h, o, n), Te(e, h.prev, h.next), Te(e, h, f === null ? e.effect.first : f.next), Te(e, f, h), f = h;
        continue;
      }
      for (p = [], d = []; o !== null && o !== h; )
        (a ??= /* @__PURE__ */ new Set()).add(o), d.push(o), o = it(o.next);
      if (o === null)
        continue;
    }
    (h.f & _e) === 0 && p.push(h), f = h, o = it(h.next);
  }
  if (e.outrogroups !== null) {
    for (const B of e.outrogroups)
      B.pending.size === 0 && (Qt(e, Rt(B.done)), e.outrogroups?.delete(B));
    e.outrogroups.size === 0 && (e.outrogroups = null);
  }
  if (o !== null || a !== void 0) {
    var V = [];
    if (a !== void 0)
      for (h of a)
        (h.f & j) === 0 && V.push(h);
    for (; o !== null; )
      (o.f & j) === 0 && o !== e.fallback && V.push(o), o = it(o.next);
    var le = V.length;
    if (le > 0) {
      var ye = s === 0 ? n : null;
      as(e, V, ye);
    }
  }
}
function us(e, t, n, r, i, s, l, o) {
  var a = (l & _i) !== 0 ? (l & mi) === 0 ? /* @__PURE__ */ zi(n, !1, !1) : qe(n) : null, f = (l & gi) !== 0 ? qe(i) : null;
  return {
    v: a,
    i: f,
    e: J(() => (s(t, a ?? n, f ?? i, o), () => {
      e.delete(r);
    }))
  };
}
function at(e, t, n) {
  if (e.nodes)
    for (var r = e.nodes.start, i = e.nodes.end, s = t && (t.f & _e) === 0 ? (
      /** @type {EffectNodes} */
      t.nodes.start
    ) : n; r !== null; ) {
      var l = (
        /** @type {TemplateNode} */
        /* @__PURE__ */ ht(r)
      );
      if (s.before(r), r === i)
        return;
      r = l;
    }
}
function Te(e, t, n) {
  t === null ? e.effect.first = n : t.next = n, n === null ? e.effect.last = t : n.prev = t;
}
function Cr(e, t) {
  ur(() => {
    var n = e.getRootNode(), r = (
      /** @type {ShadowRoot} */
      n.host ? (
        /** @type {ShadowRoot} */
        n
      ) : (
        /** @type {Document} */
        n.head ?? /** @type {Document} */
        n.ownerDocument.head
      )
    );
    if (!r.querySelector("#" + t.hash)) {
      const i = lr("style");
      i.id = t.hash, i.textContent = t.code, r.appendChild(i);
    }
  });
}
const Nn = [...` 	
\r\f \v\uFEFF`];
function cs(e, t, n) {
  var r = e == null ? "" : "" + e;
  if (n) {
    for (var i of Object.keys(n))
      if (n[i])
        r = r ? r + " " + i : i;
      else if (r.length)
        for (var s = i.length, l = 0; (l = r.indexOf(i, l)) >= 0; ) {
          var o = l + s;
          (l === 0 || Nn.includes(r[l - 1])) && (o === r.length || Nn.includes(r[o])) ? r = (l === 0 ? "" : r.substring(0, l)) + r.substring(o + 1) : l = o;
        }
  }
  return r === "" ? null : r;
}
function Ln(e, t = !1) {
  var n = t ? " !important;" : ";", r = "";
  for (var i of Object.keys(e)) {
    var s = e[i];
    s != null && s !== "" && (r += " " + i + ": " + s + n);
  }
  return r;
}
function hs(e, t) {
  if (t) {
    var n = "", r, i;
    return Array.isArray(t) ? (r = t[0], i = t[1]) : r = t, r && (n += Ln(r)), i && (n += Ln(i, !0)), n = n.trim(), n === "" ? null : n;
  }
  return String(e);
}
function mt(e, t, n, r, i, s) {
  var l = (
    /** @type {any} */
    e[Ut]
  );
  if (l !== n || l === void 0) {
    var o = cs(n, r, s);
    o == null ? e.removeAttribute("class") : t ? e.className = o : e.setAttribute("class", o), e[Ut] = n;
  } else if (s && i !== s)
    for (var a in s) {
      var f = !!s[a];
      (i == null || f !== !!i[a]) && e.classList.toggle(a, f);
    }
  return s;
}
function qt(e, t = {}, n, r) {
  for (var i in n) {
    var s = n[i];
    t[i] !== s && (n[i] == null ? e.style.removeProperty(i) : e.style.setProperty(i, s, r));
  }
}
function st(e, t, n, r) {
  var i = (
    /** @type {any} */
    e[Vt]
  );
  if (i !== t) {
    var s = hs(t, r);
    s == null ? e.removeAttribute("style") : e.style.cssText = s, e[Vt] = t;
  } else r && (Array.isArray(r) ? (qt(e, n?.[0], r[0]), qt(e, n?.[1], r[1], "important")) : qt(e, n, r));
  return r;
}
const ds = Symbol("is custom element"), vs = Symbol("is html");
function Ae(e, t, n, r) {
  var i = ps(e);
  i[t] !== (i[t] = n) && (t === "loading" && (e[si] = n), n == null ? e.removeAttribute(t) : typeof n != "string" && _s(e).includes(t) ? e[t] = n : e.setAttribute(t, n));
}
function ps(e) {
  return (
    /** @type {Record<string | symbol, unknown>} **/
    /** @type {any} */
    e[Bn] ??= {
      [ds]: e.nodeName.includes("-"),
      [vs]: e.namespaceURI === bi
    }
  );
}
var Dn = /* @__PURE__ */ new Map();
function _s(e) {
  var t = e.getAttribute("is") || e.nodeName, n = Dn.get(t);
  if (n) return n;
  Dn.set(t, n = []);
  for (var r, i = e, s = Element.prototype; s !== i; ) {
    r = Jr(i);
    for (var l in r)
      r[l].set && // better safe than sorry, we don't want spread attributes to mess with HTML content
      l !== "innerHTML" && l !== "textContent" && l !== "innerText" && n.push(l);
    i = zn(i);
  }
  return n;
}
function Bt(e, t) {
  return e === t || e?.[yt] === t;
}
function Mr(e = {}, t, n, r) {
  var i = (
    /** @type {ComponentContext} */
    q.r
  ), s = (
    /** @type {Effect} */
    E
  );
  return ur(() => {
    var l, o;
    return cr(() => {
      l = o, o = [], kr(() => {
        Bt(n(...o), e) || (t(e, ...o), l && Bt(n(...l), e) && t(null, ...l));
      });
    }), () => {
      let a = s;
      for (; a !== i && a.parent !== null && a.parent.f & Yt; )
        a = a.parent;
      const f = () => {
        o && Bt(n(...o), e) && t(null, ...o);
      }, p = a.teardown;
      a.teardown = () => {
        f(), p?.();
      };
    };
  }), e;
}
function gs(e, t, n, r) {
  var i = (
    /** @type {V} */
    r
  ), s = !0, l = () => (s && (s = !1, i = /** @type {V} */
  r), i);
  e[t];
  var o;
  o = () => {
    var d = (
      /** @type {V} */
      e[t]
    );
    return d === void 0 ? l() : (s = !0, d);
  };
  var a = !1, f = /* @__PURE__ */ Lt(() => (a = !1, o())), p = (
    /** @type {Effect} */
    E
  );
  return (
    /** @type {() => V} */
    (function(d, v) {
      if (arguments.length > 0) {
        const _ = v ? u(f) : d;
        return T(f, _), a = !0, i !== void 0 && (i = _), d;
      }
      return ge && a || (p.f & W) !== 0 ? f.v : u(f);
    })
  );
}
const ms = "5";
typeof window < "u" && ((window.__svelte ??= {}).v ??= /* @__PURE__ */ new Set()).add(ms);
const ys = { sources: [], outputs: [], links: [], groups: [] }, hn = "Audio routing";
function Rr(e) {
  return e.show_title !== !1 && (e.title ?? hn) !== "";
}
const ws = (e) => e.show_hint !== !1;
var bs = /* @__PURE__ */ K('<h1 class="header svelte-9gvyv2"> </h1>'), Es = /* @__PURE__ */ K('<p class="error svelte-9gvyv2"> </p>'), ks = /* @__PURE__ */ K('<p class="muted svelte-9gvyv2">Loading routing…</p>'), xs = /* @__PURE__ */ K('<p class="muted svelte-9gvyv2">No inputs or outputs yet. Add them in the PipeWire Audio Router add-on.</p>'), Ts = /* @__PURE__ */ is('<path></path><path class="hit svelte-9gvyv2" role="button" tabindex="0"></path>', 1), As = /* @__PURE__ */ K('<span class="sub svelte-9gvyv2">offline</span>'), Ss = /* @__PURE__ */ K('<button><span class="name svelte-9gvyv2"> </span> <!> <span class="dot right-dot svelte-9gvyv2"></span></button>'), Cs = /* @__PURE__ */ K('<span class="sub svelte-9gvyv2"> </span>'), Ms = /* @__PURE__ */ K('<button><span class="dot left-dot svelte-9gvyv2"></span> <span class="name svelte-9gvyv2"> </span> <!></button>'), Rs = /* @__PURE__ */ K('<p class="hint svelte-9gvyv2"> </p>'), Os = /* @__PURE__ */ K('<div><svg class="wires svelte-9gvyv2"></svg> <div class="col left svelte-9gvyv2"></div> <div class="col right svelte-9gvyv2"></div></div> <!>', 1), Ns = /* @__PURE__ */ K('<ha-card><!> <div class="body svelte-9gvyv2"><!> <!></div></ha-card>', 2);
const Ls = {
  hash: "svelte-9gvyv2",
  code: `
  /* Everything is expressed in Home Assistant's own theme variables, so the card
     follows the dashboard's theme (including dark mode) with no logic of ours. */.header.svelte-9gvyv2 {font-family:var(--ha-card-header-font-family, inherit);font-size:var(--ha-card-header-font-size, 24px);font-weight:normal;color:var(--ha-card-header-color, var(--primary-text-color));padding:12px 16px 4px;margin:0;letter-spacing:-0.012em;line-height:1.2;}.body.svelte-9gvyv2 {padding:8px 12px 12px;}.muted.svelte-9gvyv2,
  .hint.svelte-9gvyv2 {color:var(--secondary-text-color);font-size:12px;margin:8px 4px 0;}.error.svelte-9gvyv2 {color:var(--error-color, #db4437);font-size:13px;margin:0 4px 8px;}.canvas.svelte-9gvyv2 {position:relative;width:100%;}.canvas.busy.svelte-9gvyv2 {
    /* A call is in flight; the daemon's push is what ends it. Non-interactive
       rather than spinner-ed, so the picture never jumps. */pointer-events:none;opacity:0.65;}.wires.svelte-9gvyv2 {position:absolute;inset:0;overflow:visible;}.wire.svelte-9gvyv2 {fill:none;stroke:var(--primary-color, #03a9f4);stroke-width:2.5;stroke-linecap:round;pointer-events:none;}.wire.partial.svelte-9gvyv2 {stroke-dasharray:6 5;}.wire.off.svelte-9gvyv2 {stroke:var(--disabled-text-color, #bdbdbd);}.wire.live.svelte-9gvyv2 {stroke-width:4;}.hit.svelte-9gvyv2 {fill:none;stroke:transparent;stroke-width:16;cursor:pointer;}.hit.svelte-9gvyv2:focus-visible {outline:none;stroke:var(--primary-color, #03a9f4);stroke-opacity:0.3;}.col.svelte-9gvyv2 {position:absolute;top:0;display:flex;flex-direction:column;gap:8px; /* GAP */padding-top:6px; /* TOP */}.left.svelte-9gvyv2 {left:0;}.right.svelte-9gvyv2 {right:0;}.node.svelte-9gvyv2 {position:relative;box-sizing:border-box;display:flex;flex-direction:column;justify-content:center;gap:1px;width:100%;padding:4px 10px;border:1px solid var(--divider-color, #e0e0e0);border-radius:10px;background:var(--card-background-color, #fff);color:var(--primary-text-color);font:inherit;text-align:left;cursor:pointer;}.node.target.svelte-9gvyv2 {text-align:right;align-items:flex-end;}.node.svelte-9gvyv2:hover {border-color:var(--primary-color, #03a9f4);}.node.held.svelte-9gvyv2 {border-color:var(--primary-color, #03a9f4);box-shadow:0 0 0 1px var(--primary-color, #03a9f4);}.node.absent.svelte-9gvyv2 {color:var(--disabled-text-color, #bdbdbd);}.name.svelte-9gvyv2 {font-size:14px;line-height:1.2;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:100%;}.sub.svelte-9gvyv2 {font-size:11px;color:var(--secondary-text-color);line-height:1.1;}
  /* The wire's anchor, drawn on the edge the wires leave from so a route visibly
     starts at the row rather than floating beside it. */.dot.svelte-9gvyv2 {position:absolute;top:50%;width:8px;height:8px;margin-top:-4px;border-radius:50%;background:var(--divider-color, #e0e0e0);}.right-dot.svelte-9gvyv2 {right:-4px;}.left-dot.svelte-9gvyv2 {left:-4px;}.node.held.svelte-9gvyv2 .dot:where(.svelte-9gvyv2),
  .node.svelte-9gvyv2:hover .dot:where(.svelte-9gvyv2) {background:var(--primary-color, #03a9f4);}`
};
function Ds(e, t) {
  tn(t, !0), Cr(e, Ls);
  let n = gs(t, "model");
  const r = 6, i = 44, s = 8, l = 84, o = 220, a = 0.36, f = 56;
  let p = /* @__PURE__ */ N(void 0), d = /* @__PURE__ */ N(
    0
    // measured canvas width
  );
  const v = /* @__PURE__ */ R(() => n().snapshot), _ = /* @__PURE__ */ R(() => u(v).links), h = /* @__PURE__ */ R(() => u(v).sources), w = /* @__PURE__ */ R(() => new Map(u(v).outputs.map((c) => [c.node_name, c]))), m = /* @__PURE__ */ R(() => new Map(u(h).map((c) => [c.node_name, c]))), y = /* @__PURE__ */ R(() => {
    const c = new Set(u(v).groups.flatMap((g) => g.members));
    return [
      ...u(v).groups.map((g) => ({
        kind: "group",
        key: `g:${g.id}`,
        id: g.id,
        name: g.name,
        members: g.members
      })),
      ...u(v).outputs.filter((g) => !c.has(g.node_name)).map((g) => ({
        kind: "solo",
        key: `o:${g.node_name}`,
        name: g.display_name,
        members: [g.node_name],
        node: g
      }))
    ];
  }), b = (c) => c.members.some((g) => u(w).get(g)?.present ?? !1), D = (c, g) => c.members.filter((S) => u(_).some((te) => te.source === g && te.output === S)), U = (c) => [
    ...new Set(u(_).filter((g) => c.members.includes(g.output)).map((g) => g.source))
  ], V = /* @__PURE__ */ R(() => Math.max(l, Math.min(o, Math.round(u(d) * a), Math.floor((u(d) - f) / 2)))), le = /* @__PURE__ */ R(() => Math.max(u(V), u(d) - u(V))), ye = (c) => r + c * (i + s) + i / 2, B = (c) => c === 0 ? 0 : r * 2 + c * i + (c - 1) * s, X = /* @__PURE__ */ R(() => Math.max(i + r * 2, B(u(h).length), B(u(y).length))), Dt = /* @__PURE__ */ R(() => new Map(u(h).map((c, g) => [c.node_name, ye(g)]))), Pt = /* @__PURE__ */ R(() => new Map(u(y).map((c, g) => [c.key, ye(g)])));
  function Or(c, g, S, te) {
    const we = Math.max(28, (S - c) * 0.45);
    return `M${c},${g} C${c + we},${g} ${S - we},${te} ${S},${te}`;
  }
  const dn = /* @__PURE__ */ R(() => {
    if (u(d) === 0) return [];
    const c = [];
    for (const g of u(y))
      for (const S of U(g)) {
        const te = u(Dt).get(S), we = u(Pt).get(g.key);
        if (te === void 0 || we === void 0) continue;
        const be = D(g, S);
        c.push({
          source: S,
          target: g,
          partial: be.length !== g.members.length,
          off: !(u(m).get(S)?.present ?? !1) || !be.some((nt) => u(w).get(nt)?.present),
          path: Or(u(V), te, u(le), we)
        });
      }
    return c;
  });
  let C = /* @__PURE__ */ N(null);
  const vt = /* @__PURE__ */ R(() => {
    const c = u(C);
    return c?.kind === "target" ? u(y).find((g) => g.key === c.key) : void 0;
  });
  St(() => {
    const c = u(C);
    c?.kind === "source" && !u(m).has(c.name) && T(C, null), c?.kind === "target" && !u(y).some((g) => g.key === c.key) && T(C, null);
  });
  async function Nr(c) {
    if (u(C)?.kind === "source") {
      T(C, u(C).name === c ? null : { kind: "source", name: c }, !0);
      return;
    }
    if (u(vt)) {
      const g = u(vt);
      T(C, null), await vn(c, g);
      return;
    }
    T(C, { kind: "source", name: c }, !0);
  }
  async function Lr(c) {
    if (u(C)?.kind === "target") {
      T(C, u(C).key === c.key ? null : { kind: "target", key: c.key }, !0);
      return;
    }
    if (u(C)?.kind === "source") {
      const g = u(C).name;
      T(C, null), await vn(g, c);
      return;
    }
    T(C, { kind: "target", key: c.key }, !0);
  }
  async function vn(c, g) {
    if (g.members.length === 0) {
      n().error = `“${g.name}” has no speakers yet — add one in the add-on first.`;
      return;
    }
    const S = D(g, c);
    if (g.kind === "group") {
      S.length === g.members.length && g.members.length > 0 ? await n().unrouteGroup(g.id) : await n().routeGroup(g.id, c);
      return;
    }
    S.length ? await n().unlink(c, g.members[0]) : await n().link(c, g.members[0]);
  }
  async function pn(c) {
    if (c.target.kind === "group") {
      for (const g of D(c.target, c.source))
        await n().unlink(c.source, g);
      return;
    }
    await n().unlink(c.source, c.target.members[0]);
  }
  const Dr = (c) => `Remove route ${u(m).get(c.source)?.display_name ?? c.source} → ${c.target.name}`;
  function _n(c) {
    if (c.kind === "solo") return b(c) ? "" : "offline";
    if (c.members.length === 0) return "no speakers";
    const g = [
      `${c.members.length} speaker${c.members.length === 1 ? "" : "s"}`
    ];
    return !b(c) && c.members.length ? g.push("offline") : U(c).length > 1 && g.push("mixed"), g.join(" · ");
  }
  const Pr = /* @__PURE__ */ R(() => u(C)?.kind === "source" ? `Tap where “${u(m).get(u(C).name)?.display_name ?? u(C).name}” should play — or tap it again to cancel.` : u(vt) ? `Tap the input “${u(vt).name}” should play — or tap it again to cancel.` : "Tap an input, then where it should play. Tap a wire to remove a route."), Ft = (c) => u(C)?.kind === "source" && u(C).name === c, It = (c) => u(C)?.kind === "target" && u(C).key === c, Fr = (c) => Ft(c.source) || It(c.target.key);
  St(() => {
    const c = u(p);
    if (!c) return;
    T(d, c.clientWidth, !0);
    const g = new ResizeObserver(([S]) => {
      T(d, Math.round(S.contentRect.width), !0);
    });
    return g.observe(c), () => g.disconnect();
  });
  var gn = Ns(), mn = ne(gn);
  {
    var Ir = (c) => {
      var g = bs(), S = ne(g);
      xe(() => Ge(S, n().config.title || hn)), z(c, g);
    }, Hr = /* @__PURE__ */ R(() => Rr(n().config));
    Pe(mn, (c) => {
      u(Hr) && c(Ir);
    });
  }
  var zr = ve(mn, 2), yn = ne(zr);
  {
    var jr = (c) => {
      var g = Es(), S = ne(g);
      xe(() => Ge(S, n().error)), z(c, g);
    };
    Pe(yn, (c) => {
      n().error && c(jr);
    });
  }
  var qr = ve(yn, 2);
  {
    var Br = (c) => {
      var g = ks();
      z(c, g);
    }, Gr = (c) => {
      var g = xs();
      z(c, g);
    }, Yr = (c) => {
      var g = Os(), S = Xt(g);
      let te, we;
      var be = ne(S);
      jt(be, 21, () => u(dn), (P) => P.source + " " + P.target.key, (P, A) => {
        var F = Ts(), ce = Xt(F);
        let Ee;
        var ke = ve(ce);
        xe(
          (he, rt) => {
            Ee = mt(ce, 0, "wire svelte-9gvyv2", null, Ee, he), Ae(ce, "d", u(A).path), Ae(ke, "d", u(A).path), Ae(ke, "aria-label", rt);
          },
          [
            () => ({
              off: u(A).off,
              partial: u(A).partial,
              live: Fr(u(A))
            }),
            () => Dr(u(A))
          ]
        ), _t("click", ke, () => pn(u(A))), _t("keydown", ke, (he) => {
          (he.key === "Enter" || he.key === " ") && (he.preventDefault(), pn(u(A)));
        }), z(P, F);
      });
      var nt = ve(be, 2);
      let wn;
      jt(nt, 21, () => u(h), (P) => P.node_name, (P, A) => {
        var F = Ss();
        let ce;
        st(F, "", {}, { height: "44px" });
        var Ee = ne(F), ke = ne(Ee), he = ve(Ee, 2);
        {
          var rt = (De) => {
            var de = As();
            z(De, de);
          };
          Pe(he, (De) => {
            u(A).present || De(rt);
          });
        }
        xe(
          (De, de) => {
            ce = mt(F, 1, "node svelte-9gvyv2", null, ce, De), Ae(F, "aria-pressed", de), Ge(ke, u(A).display_name);
          },
          [
            () => ({
              absent: !u(A).present,
              held: Ft(u(A).node_name)
            }),
            () => Ft(u(A).node_name)
          ]
        ), _t("click", F, () => Nr(u(A).node_name)), z(P, F);
      });
      var bn = ve(nt, 2);
      let En;
      jt(bn, 21, () => u(y), (P) => P.key, (P, A) => {
        var F = Ms();
        let ce;
        st(F, "", {}, { height: "44px" });
        var Ee = ve(ne(F), 2), ke = ne(Ee), he = ve(Ee, 2);
        {
          var rt = (de) => {
            var pt = Cs(), Wr = ne(pt);
            xe((Kr) => Ge(Wr, Kr), [() => _n(u(A))]), z(de, pt);
          }, De = /* @__PURE__ */ R(() => _n(u(A)));
          Pe(he, (de) => {
            u(De) && de(rt);
          });
        }
        xe(
          (de, pt) => {
            ce = mt(F, 1, "node target svelte-9gvyv2", null, ce, de), Ae(F, "aria-pressed", pt), Ge(ke, u(A).name);
          },
          [
            () => ({ absent: !b(u(A)), held: It(u(A).key) }),
            () => It(u(A).key)
          ]
        ), _t("click", F, () => Lr(u(A))), z(P, F);
      }), Mr(S, (P) => T(p, P), () => u(p));
      var Ur = ve(S, 2);
      {
        var Vr = (P) => {
          var A = Rs(), F = ne(A);
          xe(() => Ge(F, u(Pr))), z(P, A);
        }, $r = /* @__PURE__ */ R(() => ws(n().config) || u(C));
        Pe(Ur, (P) => {
          u($r) && P(Vr);
        });
      }
      xe(() => {
        te = mt(S, 1, "canvas svelte-9gvyv2", null, te, { busy: n().busy }), we = st(S, "", we, { height: `${u(X) ?? ""}px` }), Ae(be, "width", u(d)), Ae(be, "height", u(X)), Ae(be, "aria-hidden", u(dn).length === 0), wn = st(nt, "", wn, { width: `${u(V) ?? ""}px` }), En = st(bn, "", En, { width: `${u(V) ?? ""}px` });
      }), z(c, g);
    };
    Pe(qr, (c) => {
      n().loaded ? u(h).length === 0 && u(y).length === 0 ? c(Gr, 1) : c(Yr, -1) : c(Br);
    });
  }
  z(e, gn), nn();
}
es(["click", "keydown"]);
var Ps = /* @__PURE__ */ K(`<p class="fallback svelte-13lj9sz">Home Assistant's form components aren't available here — use the code (YAML) editor for this card.</p>`), Fs = /* @__PURE__ */ K("<ha-form></ha-form>", 2);
const Is = {
  hash: "svelte-13lj9sz",
  code: ".fallback.svelte-13lj9sz {color:var(--secondary-text-color);font-size:14px;margin:8px 0;}"
};
function Hs(e, t) {
  tn(t, !0), Cr(e, Is);
  const n = "pipewire_audio_router", r = /* @__PURE__ */ R(() => ({
    show_title: t.model.config.show_title !== !1,
    title: t.model.config.title ?? "",
    show_hint: t.model.config.show_hint !== !1,
    entry_id: t.model.config.entry_id ?? ""
  })), i = /* @__PURE__ */ R(() => [
    { name: "show_title", selector: { boolean: {} } },
    // Only worth asking for a heading when there will be one.
    ...u(r).show_title ? [{ name: "title", selector: { text: {} } }] : [],
    { name: "show_hint", selector: { boolean: {} } },
    // A picker over this integration's config entries, so the value is a real
    // entry id and not a hand-copied string.
    {
      name: "entry_id",
      selector: { config_entry: { integration: n } }
    }
  ]), s = {
    show_title: "Show title",
    title: "Title",
    show_hint: "Show instructions",
    entry_id: "Router"
  }, l = {
    title: `Leave empty for “${hn}”.`,
    show_hint: "The line under the graph explaining how to route. It reappears by itself while an input is held.",
    entry_id: "Only needed if you have more than one PipeWire Audio Router configured."
  }, o = (m) => s[m.name] ?? m.name, a = (m) => l[m.name] ?? "";
  function f(m) {
    const y = {
      type: t.model.config.type ?? "custom:pipewire-router-card"
    };
    m.show_title === !1 && (y.show_title = !1), typeof m.title == "string" && m.title !== "" && (y.title = m.title), m.show_hint === !1 && (y.show_hint = !1), typeof m.entry_id == "string" && m.entry_id !== "" && (y.entry_id = m.entry_id), t.model.onChange(y);
  }
  let p = /* @__PURE__ */ N(void 0);
  const d = /* @__PURE__ */ R(() => !!t.model.hass && !customElements.get("ha-form"));
  St(() => {
    const m = u(p);
    m && (m.hass = t.model.hass, m.schema = u(i), m.data = u(r), m.computeLabel = o, m.computeHelper = a);
  }), St(() => {
    const m = u(p);
    if (!m) return;
    const y = (b) => {
      const D = b.detail?.value;
      D && f(D);
    };
    return m.addEventListener("value-changed", y), () => m.removeEventListener("value-changed", y);
  });
  var v = ss(), _ = Xt(v);
  {
    var h = (m) => {
      var y = Ps();
      z(m, y);
    }, w = (m) => {
      var y = Fs();
      Mr(y, (b) => T(p, b), () => u(p)), z(m, y);
    };
    Pe(_, (m) => {
      u(d) ? m(h) : t.model.hass && m(w, 1);
    });
  }
  z(e, v), nn();
}
const Pn = "pipewire_audio_router";
class zs {
  #e = /* @__PURE__ */ N(Ce(ys));
  get snapshot() {
    return u(this.#e);
  }
  set snapshot(t) {
    T(this.#e, t, !0);
  }
  #t = /* @__PURE__ */ N(Ce({}));
  get config() {
    return u(this.#t);
  }
  set config(t) {
    T(this.#t, t, !0);
  }
  #n = /* @__PURE__ */ N(!1);
  get loaded() {
    return u(this.#n);
  }
  set loaded(t) {
    T(this.#n, t, !0);
  }
  #l = /* @__PURE__ */ N(null);
  get error() {
    return u(this.#l);
  }
  set error(t) {
    T(this.#l, t, !0);
  }
  #s = /* @__PURE__ */ N(!1);
  get busy() {
    return u(this.#s);
  }
  set busy(t) {
    T(this.#s, t, !0);
  }
  #o = null;
  #r = null;
  #a = null;
  #i = 0;
  setConfig(t) {
    const n = this.config.entry_id;
    this.config = t, t.entry_id !== n && this.#o && this.#d();
  }
  /** Called by the element on every `hass` update — which in Home Assistant is
   *  every state change in the house, so this must be cheap and must not
   *  resubscribe. `connection` is stable for the life of the frontend session,
   *  so it is the thing worth comparing. */
  setHass(t) {
    this.#o = t, t.connection !== this.#r && (this.#r = t.connection, this.#d());
  }
  async #d() {
    const t = this.#o;
    if (!t) return;
    const n = ++this.#i;
    await this.#f();
    try {
      const r = await t.connection.subscribeMessage(
        (i) => {
          n === this.#i && (this.snapshot = i, this.loaded = !0, this.error = null);
        },
        { type: `${Pn}/subscribe`, ...this.#u() }
      );
      if (n !== this.#i) {
        r();
        return;
      }
      this.#a = r;
    } catch (r) {
      if (n !== this.#i) return;
      this.error = Fn(r), this.loaded = !0;
    }
  }
  async #f() {
    const t = this.#a;
    if (this.#a = null, !!t)
      try {
        await t();
      } catch {
      }
  }
  /** Detached from the DOM: drop the subscription, and forget which connection we
   *  were on so re-attaching (the same element, on the same session, when its view
   *  comes back) subscribes again instead of sitting silent. */
  disconnect() {
    this.#i++, this.#r = null, this.#f();
  }
  #u() {
    return this.config.entry_id ? { entry_id: this.config.entry_id } : {};
  }
  async #c(t, n) {
    const r = this.#o;
    if (!(!r || this.busy)) {
      this.busy = !0;
      try {
        await r.callWS({ type: `${Pn}/${t}`, ...this.#u(), ...n }), this.error = null;
      } catch (i) {
        this.error = Fn(i);
      } finally {
        this.busy = !1;
      }
    }
  }
  /** Route one source into one lone output (additive: an output can mix several). */
  link(t, n) {
    return this.#c("link", { source: t, output: n });
  }
  unlink(t, n) {
    return this.#c("unlink", { source: t, output: n });
  }
  /** Put a whole group on one source — exclusive, so this also drops whatever
   *  else its members were playing. Same call as the group's Source dropdown. */
  routeGroup(t, n) {
    return this.#c("route_group", { group_id: t, source: n });
  }
  unrouteGroup(t) {
    return this.#c("unroute_group", { group_id: t });
  }
}
function Fn(e) {
  if (typeof e == "object" && e !== null && "message" in e) {
    const t = e.message;
    if (typeof t == "string" && t) return t;
  }
  return e instanceof Error && e.message ? e.message : "Home Assistant rejected the request";
}
class js {
  #e = /* @__PURE__ */ N(Ce({}));
  get config() {
    return u(this.#e);
  }
  set config(t) {
    T(this.#e, t, !0);
  }
  #t = /* @__PURE__ */ N(null);
  get hass() {
    return u(this.#t);
  }
  set hass(t) {
    T(this.#t, t, !0);
  }
  onChange = () => {
  };
}
const Qe = "pipewire-router-card", en = `${Qe}-editor`;
class qs extends HTMLElement {
  #e = new zs();
  #t = null;
  connectedCallback() {
    this.#t || (this.#t = Ar(Ds, { target: this, props: { model: this.#e } }));
  }
  disconnectedCallback() {
    this.#t && (Sr(this.#t), this.#t = null), this.#e.disconnect();
  }
  setConfig(t) {
    this.#e.setConfig({ ...t });
  }
  /** Called by Lovelace on every Home Assistant state change — cheap by design
   *  (see `RoutingModel.setHass`). */
  set hass(t) {
    this.#e.setHass(t);
  }
  /** Masonry-view height, in Lovelace's ~50 px units: the header (when there is
   *  one) plus one per row, the taller column deciding. */
  getCardSize() {
    return this.#n();
  }
  /** Sections-view sizing. Wide by default (the card is two columns and a gutter),
   *  and as tall as its rows need. */
  getGridOptions() {
    return { columns: 12, min_columns: 6, rows: this.#n(), min_rows: 2 };
  }
  #n() {
    return (Rr(this.#e.config) ? 1 : 0) + Math.max(1, this.#l());
  }
  #l() {
    const { sources: t, outputs: n, groups: r } = this.#e.snapshot, i = new Set(r.flatMap((l) => l.members)), s = r.length + n.filter((l) => !i.has(l.node_name)).length;
    return Math.max(t.length, s);
  }
  /** What the card picker inserts. No options needed for the common case of one
   *  configured router. */
  static getStubConfig() {
    return { type: `custom:${Qe}` };
  }
  /** The visual editor behind the card's "Edit" pane. Without this, Lovelace only
   *  offers the YAML editor for a custom card. */
  static async getConfigElement() {
    return await Bs(), document.createElement(en);
  }
}
async function Bs() {
  if (!customElements.get("ha-form"))
    try {
      const e = window.loadCardHelpers;
      await (await (await e?.())?.createCardElement?.({ type: "entities", entities: [] }))?.constructor?.getConfigElement?.();
    } catch {
    }
}
class Gs extends HTMLElement {
  #e = new js();
  #t = null;
  constructor() {
    super(), this.#e.onChange = (t) => {
      this.dispatchEvent(
        new CustomEvent("config-changed", { detail: { config: t }, bubbles: !0, composed: !0 })
      );
    };
  }
  connectedCallback() {
    this.#t || (this.#t = Ar(Hs, { target: this, props: { model: this.#e } }));
  }
  disconnectedCallback() {
    this.#t && (Sr(this.#t), this.#t = null);
  }
  setConfig(t) {
    this.#e.config = { ...t };
  }
  set hass(t) {
    this.#e.hass = t;
  }
}
customElements.get(Qe) || customElements.define(Qe, qs);
customElements.get(en) || customElements.define(en, Gs);
const In = window.customCards ??= [];
In.some((e) => e.type === Qe) || In.push({
  type: Qe,
  name: "PipeWire Audio Routing",
  description: "All audio routing at a glance — tap an input, then where it should play.",
  // The picker renders a live card: it is the real graph, and reading it is
  // harmless (nothing is routed until something is tapped).
  preview: !0,
  documentationURL: "https://github.com/davidgraeff/homeassistant-audio-routing"
});
