var ln = Array.isArray, si = Array.prototype.indexOf, Mt = Array.prototype.includes, Ft = Array.from, oi = Object.defineProperty, pt = Object.getOwnPropertyDescriptor, ai = Object.getOwnPropertyDescriptors, li = Object.prototype, fi = Array.prototype, Un = Object.getPrototypeOf, Cn = Object.isExtensible;
const ui = () => {
};
function ci(e) {
  for (var t = 0; t < e.length; t++)
    e[t]();
}
function Vn() {
  var e, t, n = new Promise((r, i) => {
    e = r, t = i;
  });
  return { promise: n, resolve: e, reject: t };
}
const B = 2, et = 4, Ht = 8, $n = 1 << 24, se = 16, le = 32, Fe = 64, Kt = 128, re = 512, I = 1024, q = 2048, pe = 4096, Y = 8192, J = 16384, ot = 32768, Xt = 1 << 25, tt = 65536, Ot = 1 << 17, di = 1 << 18, at = 1 << 19, hi = 1 << 20, he = 1 << 25, $e = 65536, Rt = 1 << 21, Je = 1 << 22, De = 1 << 23, Qe = Symbol("$state"), vi = Symbol(""), Wn = Symbol("attributes"), Zt = Symbol("class"), Jt = Symbol("style"), Qt = Symbol("text"), zt = new class extends Error {
  name = "StaleReactionError";
  message = "The reaction that called `getAbortSignal()` was re-run or destroyed";
}();
function pi() {
  throw new Error("https://svelte.dev/e/async_derived_orphan");
}
function _i(e, t, n) {
  throw new Error("https://svelte.dev/e/each_key_duplicate");
}
function gi(e) {
  throw new Error("https://svelte.dev/e/effect_in_teardown");
}
function mi() {
  throw new Error("https://svelte.dev/e/effect_in_unowned_derived");
}
function yi(e) {
  throw new Error("https://svelte.dev/e/effect_orphan");
}
function wi() {
  throw new Error("https://svelte.dev/e/effect_update_depth_exceeded");
}
function bi() {
  throw new Error("https://svelte.dev/e/state_descriptors_fixed");
}
function Ei() {
  throw new Error("https://svelte.dev/e/state_prototype_fixed");
}
function ki() {
  throw new Error("https://svelte.dev/e/state_unsafe_mutation");
}
function xi() {
  throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror");
}
const Ti = 1, Ai = 2, Kn = 4, Si = 8, Ci = 16, Mi = 1, Oi = 2, P = Symbol("uninitialized"), Ri = "http://www.w3.org/1999/xhtml";
function Ni() {
  console.warn("https://svelte.dev/e/derived_inert");
}
function Li() {
  console.warn("https://svelte.dev/e/select_multiple_invalid_value");
}
function Pi() {
  console.warn("https://svelte.dev/e/svelte_boundary_reset_noop");
}
function Xn(e) {
  return e === this.v;
}
function Di(e, t) {
  return e != e ? t == t : e !== t || e !== null && typeof e == "object" || typeof e == "function";
}
function Zn(e) {
  return !Di(e, this.v);
}
let U = null;
function nt(e) {
  U = e;
}
function fn(e, t = !1, n) {
  U = {
    p: U,
    i: !1,
    c: null,
    e: null,
    s: e,
    x: null,
    r: (
      /** @type {Effect} */
      b
    ),
    l: null
  };
}
function un(e) {
  var t = (
    /** @type {ComponentContext} */
    U
  ), n = t.e;
  if (n !== null) {
    t.e = null;
    for (var r of n)
      gr(r);
  }
  return t.i = !0, U = t.p, /** @type {T} */
  {};
}
function Jn() {
  return !0;
}
let Xe = [];
function Ii() {
  var e = Xe;
  Xe = [], ci(e);
}
function Ge(e) {
  if (Xe.length === 0) {
    var t = Xe;
    queueMicrotask(() => {
      t === Xe && Ii();
    });
  }
  Xe.push(e);
}
function Qn(e) {
  var t = b;
  if (t === null)
    return E.f |= De, e;
  if ((t.f & ot) === 0 && (t.f & et) === 0)
    throw e;
  Ne(e, t);
}
function Ne(e, t) {
  if (!(t !== null && (t.f & J) !== 0)) {
    for (; t !== null; ) {
      if ((t.f & Kt) !== 0) {
        if ((t.f & ot) === 0)
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
const Fi = -7169;
function R(e, t) {
  e.f = e.f & Fi | t;
}
function cn(e) {
  (e.f & re) !== 0 || e.deps === null ? R(e, I) : R(e, pe);
}
function er(e) {
  if (e !== null)
    for (const t of e)
      (t.f & B) === 0 || (t.f & $e) === 0 || (t.f ^= $e, er(
        /** @type {Derived} */
        t.deps
      ));
}
function tr(e, t, n) {
  (e.f & q) !== 0 ? t.add(e) : (e.f & pe) !== 0 && n.add(e), er(e.deps), R(e, I);
}
function Hi(e) {
  let t = 0, n = We(0), r;
  return () => {
    pn() && (f(n), yr(() => (t === 0 && (r = Rr(() => e(() => _t(n)))), t += 1, () => {
      Ge(() => {
        t -= 1, t === 0 && (r?.(), r = void 0, _t(n));
      });
    })));
  };
}
var zi = tt | at;
function ji(e, t, n, r) {
  new qi(e, t, n, r);
}
class qi {
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
  #a;
  /** @type {Effect} */
  #s;
  /** @type {Effect | null} */
  #o = null;
  /** @type {Effect | null} */
  #r = null;
  /** @type {Effect | null} */
  #l = null;
  /** @type {DocumentFragment | null} */
  #i = null;
  #h = 0;
  #u = 0;
  #c = !1;
  /** @type {Set<Effect>} */
  #f = /* @__PURE__ */ new Set();
  /** @type {Set<Effect>} */
  #_ = /* @__PURE__ */ new Set();
  /**
   * A source containing the number of pending async deriveds/expressions.
   * Only created if `$effect.pending()` is used inside the boundary,
   * otherwise updating the source results in needless `Batch.ensure()`
   * calls followed by no-op flushes
   * @type {Source<number> | null}
   */
  #d = null;
  #m = Hi(() => (this.#d = We(this.#h), () => {
    this.#d = null;
  }));
  /**
   * @param {TemplateNode} node
   * @param {BoundaryProps} props
   * @param {((anchor: Node) => void)} children
   * @param {((error: unknown) => unknown) | undefined} [transform_error]
   */
  constructor(t, n, r, i) {
    this.#e = t, this.#n = n, this.#a = (s) => {
      var a = (
        /** @type {Effect} */
        b
      );
      a.b = this, a.f |= Kt, r(s);
    }, this.parent = /** @type {Effect} */
    b.b, this.transform_error = i ?? this.parent?.transform_error ?? ((s) => s), this.#s = _n(() => {
      this.#y();
    }, zi);
  }
  #g() {
    try {
      this.#o = ne(() => this.#a(this.#e));
    } catch (t) {
      this.error(t);
    }
  }
  /**
   * @param {unknown} error The deserialized error from the server's hydration comment
   */
  #b(t) {
    const n = this.#n.failed;
    n && (this.#l = ne(() => {
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
    t && (this.is_pending = !0, this.#r = ne(() => t(this.#e)), Ge(() => {
      var n = this.#i = document.createDocumentFragment(), r = Ie();
      n.append(r), this.#o = this.#w(() => ne(() => this.#a(r))), this.#u === 0 && (this.#e.before(n), this.#i = null, Ue(
        /** @type {Effect} */
        this.#r,
        () => {
          this.#r = null;
        }
      ), this.#v(
        /** @type {Batch} */
        T
      ));
    }));
  }
  #y() {
    try {
      if (this.is_pending = this.has_pending_snippet(), this.#u = 0, this.#h = 0, this.#o = ne(() => {
        this.#a(this.#e);
      }), this.#u > 0) {
        var t = this.#i = document.createDocumentFragment();
        mn(this.#o, t);
        const n = (
          /** @type {(anchor: Node) => void} */
          this.#n.pending
        );
        this.#r = ne(() => n(this.#e));
      } else
        this.#v(
          /** @type {Batch} */
          T
        );
    } catch (n) {
      this.error(n);
    }
  }
  /**
   * @param {Batch} batch
   */
  #v(t) {
    this.is_pending = !1, t.transfer_effects(this.#f, this.#_);
  }
  /**
   * Defer an effect inside a pending boundary until the boundary resolves
   * @param {Effect} effect
   */
  defer_effect(t) {
    tr(t, this.#f, this.#_);
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
    var n = b, r = E, i = U;
    _e(this.#s), ie(this.#s), nt(this.#s.ctx);
    try {
      return He.ensure(), t();
    } catch (s) {
      return Qn(s), null;
    } finally {
      _e(n), ie(r), nt(i);
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
    this.#u += t, this.#u === 0 && (this.#v(n), this.#r && Ue(this.#r, () => {
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
    this.#p(t, n), this.#h += t, !(!this.#d || this.#c) && (this.#c = !0, Ge(() => {
      this.#c = !1, this.#d && rt(this.#d, this.#h);
    }));
  }
  get_effect_pending() {
    return this.#m(), f(
      /** @type {Source<number>} */
      this.#d
    );
  }
  /** @param {unknown} error */
  error(t) {
    if (!this.#n.onerror && !this.#n.failed)
      throw t;
    T?.is_fork ? (this.#o && T.skip_effect(this.#o), this.#r && T.skip_effect(this.#r), this.#l && T.skip_effect(this.#l), T.oncommit(() => {
      this.#k(t);
    })) : this.#k(t);
  }
  /**
   * @param {unknown} error
   */
  #k(t) {
    this.#o && (K(this.#o), this.#o = null), this.#r && (K(this.#r), this.#r = null), this.#l && (K(this.#l), this.#l = null);
    var n = this.#n.onerror;
    let r = this.#n.failed;
    var i = !1, s = !1;
    const a = () => {
      if (i) {
        Pi();
        return;
      }
      i = !0, s && xi(), this.#l !== null && Ue(this.#l, () => {
        this.#l = null;
      }), this.#w(() => {
        this.#y();
      });
    }, l = (o) => {
      try {
        s = !0, n?.(o, a), s = !1;
      } catch (u) {
        Ne(u, this.#s && this.#s.parent);
      }
      r && (this.#l = this.#w(() => {
        try {
          return ne(() => {
            var u = (
              /** @type {Effect} */
              b
            );
            u.b = this, u.f |= Kt, r(
              this.#e,
              () => o,
              () => a
            );
          });
        } catch (u) {
          return Ne(
            u,
            /** @type {Effect} */
            this.#s.parent
          ), null;
        }
      }));
    };
    Ge(() => {
      var o;
      try {
        o = this.transform_error(t);
      } catch (u) {
        Ne(u, this.#s && this.#s.parent);
        return;
      }
      o !== null && typeof o == "object" && typeof /** @type {any} */
      o.then == "function" ? o.then(
        l,
        /** @param {unknown} e */
        (u) => Ne(u, this.#s && this.#s.parent)
      ) : l(o);
    });
  }
}
function Bi(e, t, n, r) {
  const i = jt;
  var s = e.filter((_) => !_.settled), a = t.map(i);
  if (n.length === 0 && s.length === 0) {
    r(a);
    return;
  }
  var l = (
    /** @type {Effect} */
    b
  ), o = Gi(), u = s.length === 1 ? s[0].promise : s.length > 1 ? Promise.all(s.map((_) => _.promise)) : null;
  function d(_) {
    if ((l.f & J) === 0) {
      o();
      try {
        r([...a, ..._]);
      } catch (m) {
        Ne(m, l);
      }
      Nt();
    }
  }
  var p = nr();
  if (n.length === 0) {
    u.then(() => d([])).finally(p);
    return;
  }
  function h() {
    Promise.all(n.map((_) => /* @__PURE__ */ Yi(_))).then(d).catch((_) => Ne(_, l)).finally(p);
  }
  u ? u.then(() => {
    o(), h(), Nt();
  }) : h();
}
function Gi() {
  var e = (
    /** @type {Effect} */
    b
  ), t = E, n = U, r = (
    /** @type {Batch} */
    T
  );
  return function(s = !0) {
    _e(e), ie(t), nt(n), s && (e.f & J) === 0 && (r?.activate(), r?.apply());
  };
}
function Nt(e = !0) {
  _e(null), ie(null), nt(null), e && T?.deactivate();
}
function nr() {
  var e = (
    /** @type {Effect} */
    b
  ), t = e.b, n = (
    /** @type {Batch} */
    T
  ), r = !!t?.is_rendered();
  return t?.update_pending_count(1, n), n.increment(r, e), () => {
    t?.update_pending_count(-1, n), n.decrement(r, e);
  };
}
// @__NO_SIDE_EFFECTS__
function jt(e) {
  var t = B | q;
  return b !== null && (b.f |= at), {
    ctx: U,
    deps: null,
    effects: null,
    equals: Xn,
    f: t,
    fn: e,
    reactions: null,
    rv: 0,
    v: (
      /** @type {V} */
      P
    ),
    wv: 0,
    parent: b,
    ac: null
  };
}
const dt = Symbol("obsolete");
// @__NO_SIDE_EFFECTS__
function Yi(e, t, n) {
  let r = (
    /** @type {Effect | null} */
    b
  );
  r === null && pi();
  var i = (
    /** @type {Promise<V>} */
    /** @type {unknown} */
    void 0
  ), s = We(
    /** @type {V} */
    P
  ), a = !E, l = /* @__PURE__ */ new Set();
  return is(() => {
    var o = (
      /** @type {Effect} */
      b
    ), u = Vn();
    i = u.promise;
    try {
      Promise.resolve(e()).then(u.resolve, (_) => {
        _ !== zt && u.reject(_);
      }).finally(Nt);
    } catch (_) {
      u.reject(_), Nt();
    }
    var d = (
      /** @type {Batch} */
      T
    );
    if (a) {
      if ((o.f & ot) !== 0)
        var p = nr();
      if (
        // boundary can be null if the async derived is inside an $effect.root not connected to the component render tree
        r.b?.is_rendered()
      )
        d.async_deriveds.get(o)?.reject(dt);
      else
        for (const _ of l.values())
          _.reject(dt);
      l.add(u), d.async_deriveds.set(o, u);
    }
    const h = (_, m = void 0) => {
      p?.(), l.delete(u), m !== dt && (d.activate(), m ? (s.f |= De, rt(s, m)) : ((s.f & De) !== 0 && (s.f ^= De), rt(s, _)), d.deactivate());
    };
    u.promise.then(h, (_) => h(null, _ || "unknown"));
  }), _r(() => {
    for (const o of l)
      o.reject(dt);
  }), new Promise((o) => {
    function u(d) {
      function p() {
        d === i ? o(s) : u(i);
      }
      d.then(p, p);
    }
    u(i);
  });
}
// @__NO_SIDE_EFFECTS__
function O(e) {
  const t = /* @__PURE__ */ jt(e);
  return xr(t), t;
}
// @__NO_SIDE_EFFECTS__
function Ui(e) {
  const t = /* @__PURE__ */ jt(e);
  return t.equals = Zn, t;
}
function Vi(e) {
  var t = e.effects;
  if (t !== null) {
    e.effects = null;
    for (var n = 0; n < t.length; n += 1)
      K(
        /** @type {Effect} */
        t[n]
      );
  }
}
function dn(e) {
  var t, n = b, r = e.parent;
  if (!Te && r !== null && e.v !== P && // if it was never evaluated before, it's guaranteed to fail downstream, so we try to execute instead
  (r.f & (J | Y)) !== 0)
    return Ni(), e.v;
  _e(r);
  try {
    e.f &= ~$e, Vi(e), t = Cr(e);
  } finally {
    _e(n);
  }
  return t;
}
function rr(e) {
  var t = dn(e);
  if (!e.equals(t) && (e.wv = Ar(), (!T?.is_fork || e.deps === null) && (T !== null ? (T.capture(e, t, !0), en?.capture(e, t, !0)) : e.v = t, e.deps === null))) {
    R(e, I);
    return;
  }
  Te || (oe !== null ? (pn() || T?.is_fork) && oe.set(e, t) : cn(e));
}
function $i(e) {
  if (e.effects !== null)
    for (const t of e.effects)
      (t.teardown || t.ac) && (t.teardown?.(), t.ac?.abort(zt), t.fn !== null && (t.teardown = ui), t.ac = null, gt(t, 0), gn(t));
}
function ir(e) {
  if (e.effects !== null)
    for (const t of e.effects)
      t.teardown && t.fn !== null && it(t);
}
let Ut = null, Ke = null, T = null, en = null, oe = null, tn = null, Vt = !1, Ze = null, St = null;
var Mn = 0;
let Wi = 1;
class He {
  id = Wi++;
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
  #a = /* @__PURE__ */ new Set();
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
  #l = null;
  /**
   * The root effects that need to be flushed
   * @type {Effect[]}
   */
  #i = [];
  /**
   * Effects created while this batch was active.
   * @type {Effect[]}
   */
  #h = [];
  /**
   * Deferred effects (which run after async work has completed) that are DIRTY
   * @type {Set<Effect>}
   */
  #u = /* @__PURE__ */ new Set();
  /**
   * Deferred effects that are MAYBE_DIRTY
   * @type {Set<Effect>}
   */
  #c = /* @__PURE__ */ new Set();
  /**
   * A map of branches that still exist, but will be destroyed when this batch
   * is committed — we skip over these during `process`.
   * The value contains child effects that were dirty/maybe_dirty before being reset,
   * so they can be rescheduled if the branch survives.
   * @type {Map<Effect, { d: Effect[], m: Effect[] }>}
   */
  #f = /* @__PURE__ */ new Map();
  /**
   * Inverse of #skipped_branches which we need to tell prior batches to unskip them when committing
   * @type {Set<Effect>}
   */
  #_ = /* @__PURE__ */ new Set();
  is_fork = !1;
  #d = !1;
  constructor() {
    Ke === null ? Ut = Ke = this : (Ke.#n = this, this.#t = Ke), Ke = this;
  }
  #m() {
    if (this.is_fork) return !0;
    for (const r of this.#r.keys()) {
      for (var t = r, n = !1; t.parent !== null; ) {
        if (this.#f.has(t)) {
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
    this.#f.has(t) || this.#f.set(t, { d: [], m: [] }), this.#_.delete(t);
  }
  /**
   * Remove an effect from the #skipped_branches map and reschedule
   * any tracked dirty/maybe_dirty child effects
   * @param {Effect} effect
   * @param {(e: Effect) => void} callback
   */
  unskip_effect(t, n = (r) => this.schedule(r)) {
    var r = this.#f.get(t);
    if (r) {
      this.#f.delete(t);
      for (var i of r.d)
        R(i, q), n(i);
      for (i of r.m)
        R(i, pe), n(i);
    }
    this.#_.add(t);
  }
  #g() {
    this.#e = !0, Mn++ > 1e3 && (this.#p(), Ki());
    for (const o of this.#u)
      this.#c.delete(o), R(o, q), this.schedule(o);
    for (const o of this.#c)
      R(o, pe), this.schedule(o);
    const t = this.#i;
    this.#i = [], this.apply();
    var n = Ze = [], r = [], i = St = [];
    for (const o of t)
      try {
        this.#b(o, n, r);
      } catch (u) {
        throw ar(o), this.#m() || this.discard(), u;
      }
    if (T = null, i.length > 0) {
      var s = He.ensure();
      for (const o of i)
        s.schedule(o);
    }
    if (Ze = null, St = null, this.#m()) {
      this.#v(r), this.#v(n);
      for (const [o, u] of this.#f)
        or(o, u);
      i.length > 0 && /** @type {unknown} */
      T.#g();
      return;
    }
    const a = this.#E();
    if (a) {
      this.#v(r), this.#v(n), a.#y(this);
      return;
    }
    this.#u.clear(), this.#c.clear();
    for (const o of this.#a) o(this);
    this.#a.clear(), en = this, On(r), On(n), en = null, this.#l?.resolve();
    var l = (
      /** @type {Batch | null} */
      /** @type {unknown} */
      T
    );
    if (this.#o === 0 && (this.#i.length === 0 || l !== null) && this.#p(), this.#i.length > 0)
      if (l !== null) {
        const o = l;
        o.#i.push(...this.#i.filter((u) => !o.#i.includes(u)));
      } else
        l = this;
    l !== null && l.#g();
  }
  /**
   * Traverse the effect tree, executing effects or stashing
   * them for later execution as appropriate
   * @param {Effect} root
   * @param {Effect[]} effects
   * @param {Effect[]} render_effects
   */
  #b(t, n, r) {
    t.f ^= I;
    for (var i = t.first; i !== null; ) {
      var s = i.f, a = (s & (le | Fe)) !== 0, l = a && (s & I) !== 0, o = l || (s & Y) !== 0 || this.#f.has(i);
      if (!o && i.fn !== null) {
        a ? i.f ^= I : (s & et) !== 0 ? n.push(i) : yt(i) && ((s & se) !== 0 && this.#c.add(i), it(i));
        var u = i.first;
        if (u !== null) {
          i = u;
          continue;
        }
      }
      for (; i !== null; ) {
        var d = i.next;
        if (d !== null) {
          i = d;
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
    t.async_deriveds.clear(), this.transfer_effects(t.#u, t.#c);
    const n = (r) => {
      var i = r.reactions;
      if (i !== null)
        for (const l of i) {
          var s = l.f;
          if ((s & B) !== 0)
            n(
              /** @type {Derived} */
              l
            );
          else {
            var a = (
              /** @type {Effect} */
              l
            );
            s & (Je | se) && !this.async_deriveds.has(a) && (this.#c.delete(a), R(a, q), this.schedule(a));
          }
        }
    };
    for (const r of this.current.keys())
      n(r);
    this.oncommit(() => t.discard()), t.#p(), T = this, this.#g();
  }
  /**
   * @param {Effect[]} effects
   */
  #v(t) {
    for (var n = 0; n < t.length; n += 1)
      tr(t[n], this.#u, this.#c);
  }
  /**
   * Associate a change to a given source with the current
   * batch, noting its previous and current values
   * @param {Value} source
   * @param {any} value
   * @param {boolean} [is_derived]
   */
  capture(t, n, r = !1) {
    t.v !== P && !this.previous.has(t) && this.previous.set(t, t.v), (t.f & De) === 0 && (this.current.set(t, [n, r]), oe?.set(t, n)), this.is_fork || (t.v = n);
  }
  activate() {
    T = this;
  }
  deactivate() {
    T = null, oe = null;
  }
  flush() {
    try {
      Vt = !0, T = this, this.#g();
    } finally {
      Mn = 0, tn = null, Ze = null, St = null, Vt = !1, T = null, oe = null, Ye.clear();
    }
  }
  discard() {
    for (const t of this.#s) t(this);
    this.#s.clear();
    for (const t of this.async_deriveds.values())
      t.reject(dt);
    this.#p(), this.#l?.resolve();
  }
  /**
   * @param {Effect} effect
   */
  register_created_effect(t) {
    this.#h.push(t);
  }
  #w() {
    for (let p = Ut; p !== null; p = p.#n) {
      var t = p.id < this.id, n = [];
      for (const [h, [_, m]] of this.current) {
        if (p.current.has(h)) {
          var r = (
            /** @type {[any, boolean]} */
            p.current.get(h)[0]
          );
          if (t && _ !== r)
            p.current.set(h, [_, m]);
          else
            continue;
        }
        n.push(h);
      }
      if (t)
        for (const [h, _] of this.async_deriveds) {
          const m = p.async_deriveds.get(h);
          m && _.promise.then(m.resolve).catch(m.reject);
        }
      var i = [...p.current.keys()].filter(
        (h) => !/** @type {[any, boolean]} */
        p.current.get(h)[1]
      );
      if (!(!p.#e || i.length === 0)) {
        var s = i.filter((h) => !this.current.has(h));
        if (s.length === 0)
          t && p.discard();
        else if (n.length > 0) {
          if (t)
            for (const h of this.#_)
              p.unskip_effect(h, (_) => {
                (_.f & (se | Je)) !== 0 ? p.schedule(_) : p.#v([_]);
              });
          p.activate();
          var a = /* @__PURE__ */ new Set(), l = /* @__PURE__ */ new Map();
          for (var o of n)
            sr(o, s, a, l);
          l = /* @__PURE__ */ new Map();
          var u = [...p.current].filter(([h, _]) => {
            const m = this.current.get(h);
            return m ? m[0] !== _[0] || m[1] !== _[1] : !0;
          }).map(([h]) => h);
          if (u.length > 0)
            for (const h of this.#h)
              (h.f & (J | Y | Ot)) === 0 && hn(h, u, l) && ((h.f & (Je | se)) !== 0 ? (R(h, q), p.schedule(h)) : p.#u.add(h));
          if (p.#i.length > 0 && !p.#d) {
            p.apply();
            for (var d of p.#i)
              p.#b(d, [], []);
            p.#i = [];
          }
          p.deactivate();
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
    this.#d || (this.#d = !0, Ge(() => {
      this.#d = !1, this.linked && this.flush();
    }));
  }
  /**
   * @param {Set<Effect>} dirty_effects
   * @param {Set<Effect>} maybe_dirty_effects
   */
  transfer_effects(t, n) {
    for (const r of t)
      this.#u.add(r);
    for (const r of n)
      this.#c.add(r);
    t.clear(), n.clear();
  }
  /** @param {(batch: Batch) => void} fn */
  oncommit(t) {
    this.#a.add(t);
  }
  /** @param {(batch: Batch) => void} fn */
  ondiscard(t) {
    this.#s.add(t);
  }
  settled() {
    return (this.#l ??= Vn()).promise;
  }
  static ensure() {
    if (T === null) {
      const t = T = new He();
      Vt || Ge(() => {
        t.#e || t.flush();
      });
    }
    return T;
  }
  apply() {
    {
      oe = null;
      return;
    }
  }
  /**
   *
   * @param {Effect} effect
   */
  schedule(t) {
    if (tn = t, t.b?.is_pending && (t.f & (et | Ht | $n)) !== 0 && (t.f & ot) === 0) {
      t.b.defer_effect(t);
      return;
    }
    for (var n = t; n.parent !== null; ) {
      n = n.parent;
      var r = n.f;
      if (Ze !== null && n === b && (E === null || (E.f & B) === 0))
        return;
      if ((r & (Fe | le)) !== 0) {
        if ((r & I) === 0)
          return;
        n.f ^= I;
      }
    }
    this.#i.push(n);
  }
  #p() {
    if (this.linked) {
      var t = this.#t, n = this.#n;
      t === null ? Ut = n : t.#n = n, n === null ? Ke = t : n.#t = t, this.linked = !1;
    }
  }
}
function Ki() {
  try {
    wi();
  } catch (e) {
    Ne(e, tn);
  }
}
let xe = null;
function On(e) {
  var t = e.length;
  if (t !== 0) {
    for (var n = 0; n < t; ) {
      var r = e[n++];
      if ((r.f & (J | Y)) === 0 && yt(r) && (xe = /* @__PURE__ */ new Set(), it(r), r.deps === null && r.first === null && r.nodes === null && r.teardown === null && r.ac === null && br(r), xe?.size > 0)) {
        Ye.clear();
        for (const i of xe) {
          if ((i.f & (J | Y)) !== 0) continue;
          const s = [i];
          let a = i.parent;
          for (; a !== null; )
            xe.has(a) && (xe.delete(a), s.push(a)), a = a.parent;
          for (let l = s.length - 1; l >= 0; l--) {
            const o = s[l];
            (o.f & (J | Y)) === 0 && it(o);
          }
        }
        xe.clear();
      }
    }
    xe = null;
  }
}
function sr(e, t, n, r) {
  if (!n.has(e) && (n.add(e), e.reactions !== null))
    for (const i of e.reactions) {
      const s = i.f;
      (s & B) !== 0 ? sr(
        /** @type {Derived} */
        i,
        t,
        n,
        r
      ) : (s & (Je | se)) !== 0 && (s & q) === 0 && hn(i, t, r) && (R(i, q), vn(
        /** @type {Effect} */
        i
      ));
    }
}
function hn(e, t, n) {
  const r = n.get(e);
  if (r !== void 0) return r;
  if (e.deps !== null)
    for (const i of e.deps) {
      if (Mt.call(t, i))
        return !0;
      if ((i.f & B) !== 0 && hn(
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
function vn(e) {
  T.schedule(e);
}
function or(e, t) {
  if (!((e.f & le) !== 0 && (e.f & I) !== 0)) {
    (e.f & q) !== 0 ? t.d.push(e) : (e.f & pe) !== 0 && t.m.push(e), R(e, I);
    for (var n = e.first; n !== null; )
      or(n, t), n = n.next;
  }
}
function ar(e) {
  R(e, I);
  for (var t = e.first; t !== null; )
    ar(t), t = t.next;
}
let Lt = /* @__PURE__ */ new Set();
const Ye = /* @__PURE__ */ new Map();
let lr = !1;
function We(e, t) {
  var n = {
    f: 0,
    // TODO ideally we could skip this altogether, but it causes type errors
    v: e,
    reactions: null,
    equals: Xn,
    rv: 0,
    wv: 0
  };
  return n;
}
// @__NO_SIDE_EFFECTS__
function D(e, t) {
  const n = We(e);
  return xr(n), n;
}
// @__NO_SIDE_EFFECTS__
function Xi(e, t = !1, n = !0) {
  const r = We(e);
  return t || (r.equals = Zn), r;
}
function A(e, t, n = !1) {
  E !== null && // since we are untracking the function inside `$inspect.with` we need to add this check
  // to ensure we error if state is set inside an inspect effect
  (!ae || (E.f & Ot) !== 0) && Jn() && (E.f & (B | se | Je | Ot)) !== 0 && (ve === null || !ve.has(e)) && ki();
  let r = n ? Le(t) : t;
  return rt(e, r, St);
}
function rt(e, t, n = null) {
  if (!e.equals(t)) {
    Ye.set(e, Te ? t : e.v);
    var r = He.ensure();
    if (r.capture(e, t), (e.f & B) !== 0) {
      const i = (
        /** @type {Derived} */
        e
      );
      (e.f & q) !== 0 && dn(i), oe === null && cn(i);
    }
    e.wv = Ar(), fr(e, q, n), b !== null && (b.f & I) !== 0 && (b.f & (le | Fe)) === 0 && (te === null ? as([e]) : te.push(e)), !r.is_fork && Lt.size > 0 && !lr && Zi();
  }
  return t;
}
function Zi() {
  lr = !1;
  for (const e of Lt) {
    (e.f & I) !== 0 && R(e, pe);
    let t;
    try {
      t = yt(e);
    } catch {
      t = !0;
    }
    t && it(e);
  }
  Lt.clear();
}
function _t(e) {
  A(e, e.v + 1);
}
function fr(e, t, n) {
  var r = e.reactions;
  if (r !== null)
    for (var i = r.length, s = 0; s < i; s++) {
      var a = r[s], l = a.f, o = (l & q) === 0;
      if (o && R(a, t), (l & Ot) !== 0)
        Lt.add(
          /** @type {Effect} */
          a
        );
      else if ((l & B) !== 0) {
        var u = (
          /** @type {Derived} */
          a
        );
        oe?.delete(u), (l & $e) === 0 && (l & re && (b === null || (b.f & Rt) === 0) && (a.f |= $e), fr(u, pe, n));
      } else if (o) {
        var d = (
          /** @type {Effect} */
          a
        );
        (l & se) !== 0 && xe !== null && xe.add(d), n !== null ? n.push(d) : vn(d);
      }
    }
}
function Le(e) {
  if (typeof e != "object" || e === null || Qe in e)
    return e;
  const t = Un(e);
  if (t !== li && t !== fi)
    return e;
  var n = /* @__PURE__ */ new Map(), r = ln(e), i = /* @__PURE__ */ D(0), s = Ve, a = (l) => {
    if (Ve === s)
      return l();
    var o = E, u = Ve;
    ie(null), Pn(s);
    var d = l();
    return ie(o), Pn(u), d;
  };
  return r && n.set("length", /* @__PURE__ */ D(
    /** @type {any[]} */
    e.length
  )), new Proxy(
    /** @type {any} */
    e,
    {
      defineProperty(l, o, u) {
        (!("value" in u) || u.configurable === !1 || u.enumerable === !1 || u.writable === !1) && bi();
        var d = n.get(o);
        return d === void 0 ? a(() => {
          var p = /* @__PURE__ */ D(u.value);
          return n.set(o, p), p;
        }) : A(d, u.value, !0), !0;
      },
      deleteProperty(l, o) {
        var u = n.get(o);
        if (u === void 0) {
          if (o in l) {
            const d = a(() => /* @__PURE__ */ D(P));
            n.set(o, d), _t(i);
          }
        } else
          A(u, P), _t(i);
        return !0;
      },
      get(l, o, u) {
        if (o === Qe)
          return e;
        var d = n.get(o), p = o in l;
        if (d === void 0 && (!p || pt(l, o)?.writable) && (d = a(() => {
          var _ = Le(p ? l[o] : P), m = /* @__PURE__ */ D(_);
          return m;
        }), n.set(o, d)), d !== void 0) {
          var h = f(d);
          return h === P ? void 0 : h;
        }
        return Reflect.get(l, o, u);
      },
      getOwnPropertyDescriptor(l, o) {
        var u = Reflect.getOwnPropertyDescriptor(l, o);
        if (u && "value" in u) {
          var d = n.get(o);
          d && (u.value = f(d));
        } else if (u === void 0) {
          var p = n.get(o), h = p?.v;
          if (p !== void 0 && h !== P)
            return {
              enumerable: !0,
              configurable: !0,
              value: h,
              writable: !0
            };
        }
        return u;
      },
      has(l, o) {
        if (o === Qe)
          return !0;
        var u = n.get(o), d = u !== void 0 && u.v !== P || Reflect.has(l, o);
        if (u !== void 0 || b !== null && (!d || pt(l, o)?.writable)) {
          u === void 0 && (u = a(() => {
            var h = d ? Le(l[o]) : P, _ = /* @__PURE__ */ D(h);
            return _;
          }), n.set(o, u));
          var p = f(u);
          if (p === P)
            return !1;
        }
        return d;
      },
      set(l, o, u, d) {
        var p = n.get(o), h = o in l;
        if (r && o === "length")
          for (var _ = u; _ < /** @type {Source<number>} */
          p.v; _ += 1) {
            var m = n.get(_ + "");
            m !== void 0 ? A(m, P) : _ in l && (m = a(() => /* @__PURE__ */ D(P)), n.set(_ + "", m));
          }
        if (p === void 0)
          (!h || pt(l, o)?.writable) && (p = a(() => /* @__PURE__ */ D(void 0)), A(p, Le(u)), n.set(o, p));
        else {
          h = p.v !== P;
          var w = a(() => Le(u));
          A(p, w);
        }
        var v = Reflect.getOwnPropertyDescriptor(l, o);
        if (v?.set && v.set.call(d, u), !h) {
          if (r && typeof o == "string") {
            var y = (
              /** @type {Source<number>} */
              n.get("length")
            ), C = Number(o);
            Number.isInteger(C) && C >= y.v && A(y, C + 1);
          }
          _t(i);
        }
        return !0;
      },
      ownKeys(l) {
        f(i);
        var o = Reflect.ownKeys(l).filter((p) => {
          var h = n.get(p);
          return h === void 0 || h.v !== P;
        });
        for (var [u, d] of n)
          d.v !== P && !(u in l) && o.push(u);
        return o;
      },
      setPrototypeOf() {
        Ei();
      }
    }
  );
}
function Rn(e) {
  try {
    if (e !== null && typeof e == "object" && Qe in e)
      return e[Qe];
  } catch {
  }
  return e;
}
function Ji(e, t) {
  return Object.is(Rn(e), Rn(t));
}
var Nn, ur, cr, dr;
function Qi() {
  if (Nn === void 0) {
    Nn = window, ur = /Firefox/.test(navigator.userAgent);
    var e = Element.prototype, t = Node.prototype, n = Text.prototype;
    cr = pt(t, "firstChild").get, dr = pt(t, "nextSibling").get, Cn(e) && (e[Zt] = void 0, e[Wn] = null, e[Jt] = void 0, e.__e = void 0), Cn(n) && (n[Qt] = void 0);
  }
}
function Ie(e = "") {
  return document.createTextNode(e);
}
// @__NO_SIDE_EFFECTS__
function Pe(e) {
  return (
    /** @type {TemplateNode | null} */
    cr.call(e)
  );
}
// @__NO_SIDE_EFFECTS__
function mt(e) {
  return (
    /** @type {TemplateNode | null} */
    dr.call(e)
  );
}
function $(e, t) {
  return /* @__PURE__ */ Pe(e);
}
function nn(e, t = !1) {
  {
    var n = /* @__PURE__ */ Pe(e);
    return n instanceof Comment && n.data === "" ? /* @__PURE__ */ mt(n) : n;
  }
}
function ee(e, t = 1, n = !1) {
  let r = e;
  for (; t--; )
    r = /** @type {TemplateNode} */
    /* @__PURE__ */ mt(r);
  return r;
}
function es(e) {
  e.textContent = "";
}
function hr() {
  return !1;
}
function vr(e, t, n) {
  return (
    /** @type {T extends keyof HTMLElementTagNameMap ? HTMLElementTagNameMap[T] : Element} */
    n ? document.createElement(e, { is: n }) : document.createElement(e)
  );
}
function pr(e) {
  var t = E, n = b;
  ie(null), _e(null);
  try {
    return e();
  } finally {
    ie(t), _e(n);
  }
}
function ts(e) {
  b === null && (E === null && yi(), mi()), Te && gi();
}
function ns(e, t) {
  var n = t.last;
  n === null ? t.last = t.first = e : (n.next = e, e.prev = n, t.last = e);
}
function Ae(e, t) {
  var n = b;
  n !== null && (n.f & Y) !== 0 && (e |= Y);
  var r = {
    ctx: U,
    deps: null,
    nodes: null,
    f: e | q | re,
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
  T?.register_created_effect(r);
  var i = r;
  if ((e & et) !== 0)
    Ze !== null ? Ze.push(r) : He.ensure().schedule(r);
  else if (t !== null) {
    try {
      it(r);
    } catch (a) {
      throw K(r), a;
    }
    i.deps === null && i.teardown === null && i.nodes === null && i.first === i.last && // either `null`, or a singular child
    (i.f & at) === 0 && (i = i.first, (e & se) !== 0 && (e & tt) !== 0 && i !== null && (i.f |= tt));
  }
  if (i !== null && (i.parent = n, n !== null && ns(i, n), E !== null && (E.f & B) !== 0 && (e & Fe) === 0)) {
    var s = (
      /** @type {Derived} */
      E
    );
    (s.effects ??= []).push(i);
  }
  return r;
}
function pn() {
  return E !== null && !ae;
}
function _r(e) {
  const t = Ae(Ht, null);
  return R(t, I), t.teardown = e, t;
}
function Pt(e) {
  ts();
  var t = (
    /** @type {Effect} */
    b.f
  ), n = !E && (t & le) !== 0 && U !== null && !U.i;
  if (n) {
    var r = (
      /** @type {ComponentContext} */
      U
    );
    (r.e ??= []).push(e);
  } else
    return gr(e);
}
function gr(e) {
  return Ae(et | hi, e);
}
function rs(e) {
  He.ensure();
  const t = Ae(Fe | at, e);
  return (n = {}) => new Promise((r) => {
    n.outro ? Ue(t, () => {
      K(t), r(void 0);
    }) : (K(t), r(void 0));
  });
}
function mr(e) {
  return Ae(et, e);
}
function is(e) {
  return Ae(Je | at, e);
}
function yr(e, t = 0) {
  return Ae(Ht | t, e);
}
function de(e, t = [], n = [], r = []) {
  Bi(r, t, n, (i) => {
    Ae(Ht, () => {
      e(...i.map(f));
    });
  });
}
function _n(e, t = 0) {
  var n = Ae(se | t, e);
  return n;
}
function ne(e) {
  return Ae(le | at, e);
}
function wr(e) {
  var t = e.teardown;
  if (t !== null) {
    const n = Te, r = E;
    Ln(!0), ie(null);
    try {
      t.call(null);
    } finally {
      Ln(n), ie(r);
    }
  }
}
function gn(e, t = !1) {
  var n = e.first;
  for (e.first = e.last = null; n !== null; ) {
    const i = n.ac;
    i !== null && pr(() => {
      i.abort(zt);
    });
    var r = n.next;
    (n.f & Fe) !== 0 ? n.parent = null : K(n, t), n = r;
  }
}
function ss(e) {
  for (var t = e.first; t !== null; ) {
    var n = t.next;
    (t.f & le) === 0 && K(t), t = n;
  }
}
function K(e, t = !0) {
  var n = !1;
  (t || (e.f & di) !== 0) && e.nodes !== null && e.nodes.end !== null && (os(
    e.nodes.start,
    /** @type {TemplateNode} */
    e.nodes.end
  ), n = !0), e.f |= Xt, gn(e, t && !n), gt(e, 0);
  var r = e.nodes && e.nodes.t;
  if (r !== null)
    for (const s of r)
      s.stop();
  wr(e), e.f ^= Xt, e.f |= J;
  var i = e.parent;
  i !== null && i.first !== null && br(e), e.next = e.prev = e.teardown = e.ctx = e.deps = e.fn = e.nodes = e.ac = e.b = null;
}
function os(e, t) {
  for (; e !== null; ) {
    var n = e === t ? null : /* @__PURE__ */ mt(e);
    e.remove(), e = n;
  }
}
function br(e) {
  var t = e.parent, n = e.prev, r = e.next;
  n !== null && (n.next = r), r !== null && (r.prev = n), t !== null && (t.first === e && (t.first = r), t.last === e && (t.last = n));
}
function Ue(e, t, n = !0) {
  var r = [];
  Er(e, r, !0);
  var i = () => {
    n && K(e), t && t();
  }, s = r.length;
  if (s > 0) {
    var a = () => --s || i();
    for (var l of r)
      l.out(a);
  } else
    i();
}
function Er(e, t, n) {
  if ((e.f & Y) === 0) {
    e.f ^= Y;
    var r = e.nodes && e.nodes.t;
    if (r !== null)
      for (const l of r)
        (l.is_global || n) && t.push(l);
    for (var i = e.first; i !== null; ) {
      var s = i.next;
      if ((i.f & Fe) === 0) {
        var a = (i.f & tt) !== 0 || // If this is a branch effect without a block effect parent,
        // it means the parent block effect was pruned. In that case,
        // transparency information was transferred to the branch effect.
        (i.f & le) !== 0 && (e.f & se) !== 0;
        Er(i, t, a ? n : !1);
      }
      i = s;
    }
  }
}
function Dt(e) {
  kr(e, !0);
}
function kr(e, t) {
  if ((e.f & Y) !== 0) {
    e.f ^= Y, (e.f & I) === 0 && (R(e, q), He.ensure().schedule(e));
    for (var n = e.first; n !== null; ) {
      var r = n.next, i = (n.f & tt) !== 0 || (n.f & le) !== 0;
      kr(n, i ? t : !1), n = r;
    }
    var s = e.nodes && e.nodes.t;
    if (s !== null)
      for (const a of s)
        (a.is_global || t) && a.in();
  }
}
function mn(e, t) {
  if (e.nodes)
    for (var n = e.nodes.start, r = e.nodes.end; n !== null; ) {
      var i = n === r ? null : /* @__PURE__ */ mt(n);
      t.append(n), n = i;
    }
}
let Ct = !1, Te = !1;
function Ln(e) {
  Te = e;
}
let E = null, ae = !1;
function ie(e) {
  E = e;
}
let b = null;
function _e(e) {
  b = e;
}
let ve = null;
function xr(e) {
  E !== null && (ve ??= /* @__PURE__ */ new Set()).add(e);
}
let W = null, Z = 0, te = null;
function as(e) {
  te = e;
}
let Tr = 1, Be = 0, Ve = Be;
function Pn(e) {
  Ve = e;
}
function Ar() {
  return ++Tr;
}
function yt(e) {
  var t = e.f;
  if ((t & q) !== 0)
    return !0;
  if (t & B && (e.f &= ~$e), (t & pe) !== 0) {
    for (var n = (
      /** @type {Value[]} */
      e.deps
    ), r = n.length, i = 0; i < r; i++) {
      var s = n[i];
      if (yt(
        /** @type {Derived} */
        s
      ) && rr(
        /** @type {Derived} */
        s
      ), s.wv > e.wv)
        return !0;
    }
    (t & re) !== 0 && // During time traveling we don't want to reset the status so that
    // traversal of the graph in the other batches still happens
    oe === null && R(e, I);
  }
  return !1;
}
function Sr(e, t, n = !0) {
  var r = e.reactions;
  if (r !== null && !(ve !== null && ve.has(e)))
    for (var i = 0; i < r.length; i++) {
      var s = r[i];
      (s.f & B) !== 0 ? Sr(
        /** @type {Derived} */
        s,
        t,
        !1
      ) : t === s && (n ? R(s, q) : (s.f & I) !== 0 && R(s, pe), vn(
        /** @type {Effect} */
        s
      ));
    }
}
function Cr(e) {
  var t = W, n = Z, r = te, i = E, s = ve, a = U, l = ae, o = Ve, u = e.f;
  W = /** @type {null | Value[]} */
  null, Z = 0, te = null, E = (u & (le | Fe)) === 0 ? e : null, ve = null, nt(e.ctx), ae = !1, Ve = ++Be, e.ac !== null && (pr(() => {
    e.ac.abort(zt);
  }), e.ac = null);
  try {
    e.f |= Rt;
    var d = (
      /** @type {Function} */
      e.fn
    ), p = d();
    e.f |= ot;
    var h = e.deps, _ = T?.is_fork;
    if (W !== null) {
      var m;
      if (_ || gt(e, Z), h !== null && Z > 0)
        for (h.length = Z + W.length, m = 0; m < W.length; m++)
          h[Z + m] = W[m];
      else
        e.deps = h = W;
      if (pn() && (e.f & re) !== 0)
        for (m = Z; m < h.length; m++)
          (h[m].reactions ??= []).push(e);
    } else !_ && h !== null && Z < h.length && (gt(e, Z), h.length = Z);
    if (Jn() && te !== null && !ae && h !== null && (e.f & (B | pe | q)) === 0)
      for (m = 0; m < /** @type {Source[]} */
      te.length; m++)
        Sr(
          te[m],
          /** @type {Effect} */
          e
        );
    if (i !== null && i !== e) {
      if (Be++, i.deps !== null)
        for (let w = 0; w < n; w += 1)
          i.deps[w].rv = Be;
      if (t !== null)
        for (const w of t)
          w.rv = Be;
      te !== null && (r === null ? r = te : r.push(.../** @type {Source[]} */
      te));
    }
    return (e.f & De) !== 0 && (e.f ^= De), p;
  } catch (w) {
    return Qn(w);
  } finally {
    e.f ^= Rt, W = t, Z = n, te = r, E = i, ve = s, nt(a), ae = l, Ve = o;
  }
}
function ls(e, t) {
  let n = t.reactions;
  if (n !== null) {
    var r = si.call(n, e);
    if (r !== -1) {
      var i = n.length - 1;
      i === 0 ? n = t.reactions = null : (n[r] = n[i], n.pop());
    }
  }
  if (n === null && (t.f & B) !== 0 && // Destroying a child effect while updating a parent effect can cause a dependency to appear
  // to be unused, when in fact it is used by the currently-updating parent. Checking `new_deps`
  // allows us to skip the expensive work of disconnecting and immediately reconnecting it
  (W === null || !Mt.call(W, t))) {
    var s = (
      /** @type {Derived} */
      t
    );
    (s.f & re) !== 0 && (s.f ^= re, s.f &= ~$e), s.v !== P && cn(s), $i(s), gt(s, 0);
  }
}
function gt(e, t) {
  var n = e.deps;
  if (n !== null)
    for (var r = t; r < n.length; r++)
      ls(e, n[r]);
}
function it(e) {
  var t = e.f;
  if ((t & J) === 0) {
    R(e, I);
    var n = b, r = Ct;
    b = e, Ct = !0;
    try {
      (t & (se | $n)) !== 0 ? ss(e) : gn(e), wr(e);
      var i = Cr(e);
      e.teardown = typeof i == "function" ? i : null, e.wv = Tr;
      var s;
    } finally {
      Ct = r, b = n;
    }
  }
}
function f(e) {
  var t = e.f, n = (t & B) !== 0;
  if (E !== null && !ae) {
    var r = b !== null && (b.f & J) !== 0;
    if (!r && (ve === null || !ve.has(e))) {
      var i = E.deps;
      if ((E.f & Rt) !== 0)
        e.rv < Be && (e.rv = Be, W === null && i !== null && i[Z] === e ? Z++ : W === null ? W = [e] : W.push(e));
      else {
        E.deps ??= [], Mt.call(E.deps, e) || E.deps.push(e);
        var s = e.reactions;
        s === null ? e.reactions = [E] : Mt.call(s, E) || s.push(E);
      }
    }
  }
  if (Te && Ye.has(e))
    return Ye.get(e);
  if (n) {
    var a = (
      /** @type {Derived} */
      e
    );
    if (Te) {
      var l = a.v;
      return ((a.f & I) === 0 && a.reactions !== null || Or(a)) && (l = dn(a)), Ye.set(a, l), l;
    }
    var o = (a.f & re) === 0 && !ae && E !== null && (Ct || (E.f & re) !== 0), u = (a.f & ot) === 0;
    yt(a) && (o && (a.f |= re), rr(a)), o && !u && (ir(a), Mr(a));
  }
  if (oe?.has(e))
    return oe.get(e);
  if ((e.f & De) !== 0)
    throw e.v;
  return e.v;
}
function Mr(e) {
  if (e.f |= re, e.deps !== null)
    for (const t of e.deps)
      (t.reactions ??= []).push(e), (t.f & B) !== 0 && (t.f & re) === 0 && (ir(
        /** @type {Derived} */
        t
      ), Mr(
        /** @type {Derived} */
        t
      ));
}
function Or(e) {
  if (e.v === P) return !0;
  if (e.deps === null) return !1;
  for (const t of e.deps)
    if (Ye.has(t) || (t.f & B) !== 0 && Or(
      /** @type {Derived} */
      t
    ))
      return !0;
  return !1;
}
function Rr(e) {
  var t = ae;
  try {
    return ae = !0, e();
  } finally {
    ae = t;
  }
}
const fs = ["touchstart", "touchmove"];
function us(e) {
  return fs.includes(e);
}
const ht = Symbol("events"), Nr = /* @__PURE__ */ new Set(), rn = /* @__PURE__ */ new Set();
function ft(e, t, n) {
  (t[ht] ??= {})[e] = n;
}
function cs(e) {
  for (var t = 0; t < e.length; t++)
    Nr.add(e[t]);
  for (var n of rn)
    n(e);
}
let Dn = null;
function In(e) {
  var t = this, n = (
    /** @type {Node} */
    t.ownerDocument
  ), r = e.type, i = e.composedPath?.() || [], s = (
    /** @type {null | Element} */
    i[0] || e.target
  );
  Dn = e;
  var a = 0, l = Dn === e && e[ht];
  if (l) {
    var o = i.indexOf(l);
    if (o !== -1 && (t === document || t === /** @type {any} */
    window)) {
      e[ht] = t;
      return;
    }
    var u = i.indexOf(t);
    if (u === -1)
      return;
    o <= u && (a = o);
  }
  if (s = /** @type {Element} */
  i[a] || e.target, s !== t) {
    oi(e, "currentTarget", {
      configurable: !0,
      get() {
        return s || n;
      }
    });
    var d = E, p = b;
    ie(null), _e(null);
    try {
      for (var h, _ = []; s !== null && s !== t; ) {
        try {
          var m = s[ht]?.[r];
          m != null && (!/** @type {any} */
          s.disabled || // DOM could've been updated already by the time this is reached, so we check this as well
          // -> the target could not have been disabled because it emits the event in the first place
          e.target === s) && m.call(s, e);
        } catch (w) {
          h ? _.push(w) : h = w;
        }
        if (e.cancelBubble) break;
        a++, s = a < i.length ? (
          /** @type {Element} */
          i[a]
        ) : null;
      }
      if (h) {
        for (let w of _)
          queueMicrotask(() => {
            throw w;
          });
        throw h;
      }
    } finally {
      e[ht] = t, delete e.currentTarget, ie(d), _e(p);
    }
  }
}
const ds = (
  // We gotta write it like this because after downleveling the pure comment may end up in the wrong location
  globalThis?.window?.trustedTypes && /* @__PURE__ */ globalThis.window.trustedTypes.createPolicy("svelte-trusted-html", {
    /** @param {string} html */
    createHTML: (e) => e
  })
);
function hs(e) {
  return (
    /** @type {string} */
    ds?.createHTML(e) ?? e
  );
}
function Lr(e) {
  var t = vr("template");
  return t.innerHTML = hs(e.replaceAll("<!>", "<!---->")), t.content;
}
function It(e, t) {
  var n = (
    /** @type {Effect} */
    b
  );
  n.nodes === null && (n.nodes = { start: e, end: t, a: null, t: null });
}
// @__NO_SIDE_EFFECTS__
function G(e, t) {
  var n = (t & Mi) !== 0, r = (t & Oi) !== 0, i, s = !e.startsWith("<!>");
  return () => {
    i === void 0 && (i = Lr(s ? e : "<!>" + e), n || (i = /** @type {TemplateNode} */
    /* @__PURE__ */ Pe(i)));
    var a = (
      /** @type {TemplateNode} */
      r || ur ? document.importNode(i, !0) : i.cloneNode(!0)
    );
    if (n) {
      var l = (
        /** @type {TemplateNode} */
        /* @__PURE__ */ Pe(a)
      ), o = (
        /** @type {TemplateNode} */
        a.lastChild
      );
      It(l, o);
    } else
      It(a, a);
    return a;
  };
}
// @__NO_SIDE_EFFECTS__
function vs(e, t, n = "svg") {
  var r = !e.startsWith("<!>"), i = `<${n}>${r ? e : "<!>" + e}</${n}>`, s;
  return () => {
    if (!s) {
      var a = (
        /** @type {DocumentFragment} */
        Lr(i)
      ), l = (
        /** @type {Element} */
        /* @__PURE__ */ Pe(a)
      );
      for (s = document.createDocumentFragment(); /* @__PURE__ */ Pe(l); )
        s.appendChild(
          /** @type {TemplateNode} */
          /* @__PURE__ */ Pe(l)
        );
    }
    var o = (
      /** @type {TemplateNode} */
      s.cloneNode(!0)
    );
    {
      var u = (
        /** @type {TemplateNode} */
        /* @__PURE__ */ Pe(o)
      ), d = (
        /** @type {TemplateNode} */
        o.lastChild
      );
      It(u, d);
    }
    return o;
  };
}
// @__NO_SIDE_EFFECTS__
function ps(e, t) {
  return /* @__PURE__ */ vs(e, t, "svg");
}
function _s() {
  var e = document.createDocumentFragment(), t = document.createComment(""), n = Ie();
  return e.append(t, n), It(t, n), e;
}
function z(e, t) {
  e !== null && e.before(
    /** @type {Node} */
    t
  );
}
function qe(e, t) {
  var n = t == null ? "" : typeof t == "object" ? `${t}` : t;
  n !== /** @type {any} */
  (e[Qt] ??= e.nodeValue) && (e[Qt] = n, e.nodeValue = `${n}`);
}
function Pr(e, t) {
  return gs(e, t);
}
const xt = /* @__PURE__ */ new Map();
function gs(e, { target: t, anchor: n, props: r = {}, events: i, context: s, intro: a = !0, transformError: l }) {
  Qi();
  var o = void 0, u = rs(() => {
    var d = n ?? t.appendChild(Ie());
    ji(
      /** @type {TemplateNode} */
      d,
      {
        pending: () => {
        }
      },
      (_) => {
        fn({});
        var m = (
          /** @type {ComponentContext} */
          U
        );
        s && (m.c = s), i && (r.$$events = i), o = e(_, r) || {}, un();
      },
      l
    );
    var p = /* @__PURE__ */ new Set(), h = (_) => {
      for (var m = 0; m < _.length; m++) {
        var w = _[m];
        if (!p.has(w)) {
          p.add(w);
          var v = us(w);
          for (const x of [t, document]) {
            var y = xt.get(x);
            y === void 0 && (y = /* @__PURE__ */ new Map(), xt.set(x, y));
            var C = y.get(w);
            C === void 0 ? (x.addEventListener(w, In, { passive: v }), y.set(w, 1)) : y.set(w, C + 1);
          }
        }
      }
    };
    return h(Ft(Nr)), rn.add(h), () => {
      for (var _ of p)
        for (const v of [t, document]) {
          var m = (
            /** @type {Map<string, number>} */
            xt.get(v)
          ), w = (
            /** @type {number} */
            m.get(_)
          );
          --w == 0 ? (v.removeEventListener(_, In), m.delete(_), m.size === 0 && xt.delete(v)) : m.set(_, w);
        }
      rn.delete(h), d !== n && d.parentNode?.removeChild(d);
    };
  });
  return sn.set(o, u), o;
}
let sn = /* @__PURE__ */ new WeakMap();
function Dr(e, t) {
  const n = sn.get(e);
  return n ? (sn.delete(e), n(t)) : Promise.resolve();
}
class ms {
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
  #a = /* @__PURE__ */ new Set();
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
        Dt(r), this.#a.delete(n);
      else {
        var i = this.#n.get(n);
        i && (Dt(i.effect), this.#t.set(n, i.effect), this.#n.delete(n), i.fragment.lastChild.remove(), this.anchor.before(i.fragment), r = i.effect);
      }
      for (const [s, a] of this.#e) {
        if (this.#e.delete(s), s === t)
          break;
        const l = this.#n.get(a);
        l && (K(l.effect), this.#n.delete(a));
      }
      for (const [s, a] of this.#t) {
        if (s === n || this.#a.has(s)) continue;
        const l = () => {
          if (Array.from(this.#e.values()).includes(s)) {
            var u = document.createDocumentFragment();
            mn(a, u), u.append(Ie()), this.#n.set(s, { effect: a, fragment: u });
          } else
            K(a);
          this.#a.delete(s), this.#t.delete(s);
        };
        this.#s || !r ? (this.#a.add(s), Ue(a, l, !1)) : l();
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
      n.includes(r) || (K(i.effect), this.#n.delete(r));
  };
  /**
   *
   * @param {any} key
   * @param {null | ((target: TemplateNode) => void)} fn
   */
  ensure(t, n) {
    var r = (
      /** @type {Batch} */
      T
    ), i = hr();
    if (n && !this.#t.has(t) && !this.#n.has(t))
      if (i) {
        var s = document.createDocumentFragment(), a = Ie();
        s.append(a), this.#n.set(t, {
          effect: ne(() => n(a)),
          fragment: s
        });
      } else
        this.#t.set(
          t,
          ne(() => n(this.anchor))
        );
    if (this.#e.set(r, t), i) {
      for (const [l, o] of this.#t)
        l === t ? r.unskip_effect(o) : r.skip_effect(o);
      for (const [l, o] of this.#n)
        l === t ? r.unskip_effect(o.effect) : r.skip_effect(o.effect);
      r.oncommit(this.#o), r.ondiscard(this.#r);
    } else
      this.#o(r);
  }
}
function ke(e, t, n = !1) {
  var r = new ms(e), i = n ? tt : 0;
  function s(a, l) {
    r.ensure(a, l);
  }
  _n(() => {
    var a = !1;
    t((l, o = 0) => {
      a = !0, s(o, l);
    }), a || s(-1, null);
  }, i);
}
function ys(e, t, n) {
  for (var r = [], i = t.length, s, a = t.length, l = 0; l < i; l++) {
    let p = t[l];
    Ue(
      p,
      () => {
        if (s) {
          if (s.pending.delete(p), s.done.add(p), s.pending.size === 0) {
            var h = (
              /** @type {Set<EachOutroGroup>} */
              e.outrogroups
            );
            on(e, Ft(s.done)), h.delete(s), h.size === 0 && (e.outrogroups = null);
          }
        } else
          a -= 1;
      },
      !1
    );
  }
  if (a === 0) {
    var o = r.length === 0 && n !== null;
    if (o) {
      var u = (
        /** @type {Element} */
        n
      ), d = (
        /** @type {Element} */
        u.parentNode
      );
      es(d), d.append(u), e.items.clear();
    }
    on(e, t, !o);
  } else
    s = {
      pending: new Set(t),
      done: /* @__PURE__ */ new Set()
    }, (e.outrogroups ??= /* @__PURE__ */ new Set()).add(s);
}
function on(e, t, n = !0) {
  var r;
  if (e.pending.size > 0) {
    r = /* @__PURE__ */ new Set();
    for (const a of e.pending.values())
      for (const l of a)
        r.add(
          /** @type {EachItem} */
          e.items.get(l).e
        );
  }
  for (var i = 0; i < t.length; i++) {
    var s = t[i];
    if (r?.has(s)) {
      s.f |= he;
      const a = document.createDocumentFragment();
      mn(s, a);
    } else
      K(t[i], n);
  }
}
var Fn;
function Tt(e, t, n, r, i, s = null) {
  var a = e, l = /* @__PURE__ */ new Map(), o = (t & Kn) !== 0;
  if (o) {
    var u = (
      /** @type {Element} */
      e
    );
    a = u.appendChild(Ie());
  }
  var d = null, p = /* @__PURE__ */ Ui(() => {
    var x = n();
    return (
      /** @type {V[]} */
      ln(x) ? x : x == null ? [] : Ft(x)
    );
  }), h, _ = /* @__PURE__ */ new Map(), m = !0;
  function w(x) {
    (C.effect.f & J) === 0 && (C.pending.delete(x), C.fallback = d, ws(C, h, a, t, r), d !== null && (h.length === 0 ? (d.f & he) === 0 ? Dt(d) : (d.f ^= he, vt(d, null, a)) : Ue(d, () => {
      d = null;
    })));
  }
  function v(x) {
    C.pending.delete(x);
  }
  var y = _n(() => {
    h = /** @type {V[]} */
    f(p);
    for (var x = h.length, j = /* @__PURE__ */ new Set(), X = (
      /** @type {Batch} */
      T
    ), ge = hr(), N = 0; N < x; N += 1) {
      var me = h[N], ye = r(me, N), L = m ? null : l.get(ye);
      L ? (L.v && rt(L.v, me), L.i && rt(L.i, N), ge && X.unskip_effect(L.e)) : (L = bs(
        l,
        m ? a : Fn ??= Ie(),
        me,
        ye,
        N,
        i,
        t,
        n
      ), m || (L.e.f |= he), l.set(ye, L)), j.add(ye);
    }
    if (x === 0 && s && !d && (m ? d = ne(() => s(a)) : (d = ne(() => s(Fn ??= Ie())), d.f |= he)), x > j.size && _i(), !m)
      if (_.set(X, j), ge) {
        for (const [wt, qt] of l)
          j.has(wt) || X.skip_effect(qt.e);
        X.oncommit(w), X.ondiscard(v);
      } else
        w(X);
    f(p);
  }), C = { effect: y, items: l, pending: _, outrogroups: null, fallback: d };
  m = !1;
}
function ut(e) {
  for (; e !== null && (e.f & le) === 0; )
    e = e.next;
  return e;
}
function ws(e, t, n, r, i) {
  var s = (r & Si) !== 0, a = t.length, l = e.items, o = ut(e.effect.first), u, d = null, p, h = [], _ = [], m, w, v, y;
  if (s)
    for (y = 0; y < a; y += 1)
      m = t[y], w = i(m, y), v = /** @type {EachItem} */
      l.get(w).e, (v.f & he) === 0 && (v.nodes?.a?.measure(), (p ??= /* @__PURE__ */ new Set()).add(v));
  for (y = 0; y < a; y += 1) {
    if (m = t[y], w = i(m, y), v = /** @type {EachItem} */
    l.get(w).e, e.outrogroups !== null)
      for (const L of e.outrogroups)
        L.pending.delete(v), L.done.delete(v);
    if ((v.f & Y) !== 0 && (Dt(v), s && (v.nodes?.a?.unfix(), (p ??= /* @__PURE__ */ new Set()).delete(v))), (v.f & he) !== 0)
      if (v.f ^= he, v === o)
        vt(v, null, n);
      else {
        var C = d ? d.next : o;
        v === e.effect.last && (e.effect.last = v.prev), v.prev && (v.prev.next = v.next), v.next && (v.next.prev = v.prev), Oe(e, d, v), Oe(e, v, C), vt(v, C, n), d = v, h = [], _ = [], o = ut(d.next);
        continue;
      }
    if (v !== o) {
      if (u !== void 0 && u.has(v)) {
        if (h.length < _.length) {
          var x = _[0], j;
          d = x.prev;
          var X = h[0], ge = h[h.length - 1];
          for (j = 0; j < h.length; j += 1)
            vt(h[j], x, n);
          for (j = 0; j < _.length; j += 1)
            u.delete(_[j]);
          Oe(e, X.prev, ge.next), Oe(e, d, X), Oe(e, ge, x), o = x, d = ge, y -= 1, h = [], _ = [];
        } else
          u.delete(v), vt(v, o, n), Oe(e, v.prev, v.next), Oe(e, v, d === null ? e.effect.first : d.next), Oe(e, d, v), d = v;
        continue;
      }
      for (h = [], _ = []; o !== null && o !== v; )
        (u ??= /* @__PURE__ */ new Set()).add(o), _.push(o), o = ut(o.next);
      if (o === null)
        continue;
    }
    (v.f & he) === 0 && h.push(v), d = v, o = ut(v.next);
  }
  if (e.outrogroups !== null) {
    for (const L of e.outrogroups)
      L.pending.size === 0 && (on(e, Ft(L.done)), e.outrogroups?.delete(L));
    e.outrogroups.size === 0 && (e.outrogroups = null);
  }
  if (o !== null || u !== void 0) {
    var N = [];
    if (u !== void 0)
      for (v of u)
        (v.f & Y) === 0 && N.push(v);
    for (; o !== null; )
      (o.f & Y) === 0 && o !== e.fallback && N.push(o), o = ut(o.next);
    var me = N.length;
    if (me > 0) {
      var ye = (r & Kn) !== 0 && a === 0 ? n : null;
      if (s) {
        for (y = 0; y < me; y += 1)
          N[y].nodes?.a?.measure();
        for (y = 0; y < me; y += 1)
          N[y].nodes?.a?.fix();
      }
      ys(e, N, ye);
    }
  }
  s && Ge(() => {
    if (p !== void 0)
      for (v of p)
        v.nodes?.a?.apply();
  });
}
function bs(e, t, n, r, i, s, a, l) {
  var o = (a & Ti) !== 0 ? (a & Ci) === 0 ? /* @__PURE__ */ Xi(n, !1, !1) : We(n) : null, u = (a & Ai) !== 0 ? We(i) : null;
  return {
    v: o,
    i: u,
    e: ne(() => (s(t, o ?? n, u ?? i, l), () => {
      e.delete(r);
    }))
  };
}
function vt(e, t, n) {
  if (e.nodes)
    for (var r = e.nodes.start, i = e.nodes.end, s = t && (t.f & he) === 0 ? (
      /** @type {EffectNodes} */
      t.nodes.start
    ) : n; r !== null; ) {
      var a = (
        /** @type {TemplateNode} */
        /* @__PURE__ */ mt(r)
      );
      if (s.before(r), r === i)
        return;
      r = a;
    }
}
function Oe(e, t, n) {
  t === null ? e.effect.first = n : t.next = n, n === null ? e.effect.last = t : n.prev = t;
}
function Ir(e, t) {
  mr(() => {
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
      const i = vr("style");
      i.id = t.hash, i.textContent = t.code, r.appendChild(i);
    }
  });
}
const Hn = [...` 	
\r\f \v\uFEFF`];
function Es(e, t, n) {
  var r = e == null ? "" : "" + e;
  if (n) {
    for (var i of Object.keys(n))
      if (n[i])
        r = r ? r + " " + i : i;
      else if (r.length)
        for (var s = i.length, a = 0; (a = r.indexOf(i, a)) >= 0; ) {
          var l = a + s;
          (a === 0 || Hn.includes(r[a - 1])) && (l === r.length || Hn.includes(r[l])) ? r = (a === 0 ? "" : r.substring(0, a)) + r.substring(l + 1) : a = l;
        }
  }
  return r === "" ? null : r;
}
function zn(e, t = !1) {
  var n = t ? " !important;" : ";", r = "";
  for (var i of Object.keys(e)) {
    var s = e[i];
    s != null && s !== "" && (r += " " + i + ": " + s + n);
  }
  return r;
}
function ks(e, t) {
  if (t) {
    var n = "", r, i;
    return Array.isArray(t) ? (r = t[0], i = t[1]) : r = t, r && (n += zn(r)), i && (n += zn(i, !0)), n = n.trim(), n === "" ? null : n;
  }
  return String(e);
}
function At(e, t, n, r, i, s) {
  var a = (
    /** @type {any} */
    e[Zt]
  );
  if (a !== n || a === void 0) {
    var l = Es(n, r, s);
    l == null ? e.removeAttribute("class") : t ? e.className = l : e.setAttribute("class", l), e[Zt] = n;
  } else if (s && i !== s)
    for (var o in s) {
      var u = !!s[o];
      (i == null || u !== !!i[o]) && e.classList.toggle(o, u);
    }
  return s;
}
function $t(e, t = {}, n, r) {
  for (var i in n) {
    var s = n[i];
    t[i] !== s && (n[i] == null ? e.style.removeProperty(i) : e.style.setProperty(i, s, r));
  }
}
function ct(e, t, n, r) {
  var i = (
    /** @type {any} */
    e[Jt]
  );
  if (i !== t) {
    var s = ks(t, r);
    s == null ? e.removeAttribute("style") : e.style.cssText = s, e[Jt] = t;
  } else r && (Array.isArray(r) ? ($t(e, n?.[0], r[0]), $t(e, n?.[1], r[1], "important")) : $t(e, n, r));
  return r;
}
function Fr(e, t, n = !1) {
  if (e.multiple) {
    if (t == null)
      return;
    if (!ln(t))
      return Li();
    for (var r of e.options)
      r.selected = t.includes(jn(r));
    return;
  }
  for (r of e.options) {
    var i = jn(r);
    if (Ji(i, t)) {
      r.selected = !0;
      return;
    }
  }
  (!n || t !== void 0) && (e.selectedIndex = -1);
}
function xs(e) {
  var t = new MutationObserver(() => {
    Fr(e, e.__value);
  });
  t.observe(e, {
    // Listen to option element changes
    childList: !0,
    subtree: !0,
    // because of <optgroup>
    // Listen to option element value attribute changes
    // (doesn't get notified of select value changes,
    // because that property is not reflected as an attribute)
    attributes: !0,
    attributeFilter: ["value"]
  }), _r(() => {
    t.disconnect();
  });
}
function jn(e) {
  return "__value" in e ? e.__value : e.value;
}
const Ts = Symbol("is custom element"), As = Symbol("is html");
function Re(e, t, n, r) {
  var i = Ss(e);
  i[t] !== (i[t] = n) && (t === "loading" && (e[vi] = n), n == null ? e.removeAttribute(t) : typeof n != "string" && Cs(e).includes(t) ? e[t] = n : e.setAttribute(t, n));
}
function Ss(e) {
  return (
    /** @type {Record<string | symbol, unknown>} **/
    /** @type {any} */
    e[Wn] ??= {
      [Ts]: e.nodeName.includes("-"),
      [As]: e.namespaceURI === Ri
    }
  );
}
var qn = /* @__PURE__ */ new Map();
function Cs(e) {
  var t = e.getAttribute("is") || e.nodeName, n = qn.get(t);
  if (n) return n;
  qn.set(t, n = []);
  for (var r, i = e, s = Element.prototype; s !== i; ) {
    r = ai(i);
    for (var a in r)
      r[a].set && // better safe than sorry, we don't want spread attributes to mess with HTML content
      a !== "innerHTML" && a !== "textContent" && a !== "innerText" && n.push(a);
    i = Un(i);
  }
  return n;
}
function Wt(e, t) {
  return e === t || e?.[Qe] === t;
}
function Hr(e = {}, t, n, r) {
  var i = (
    /** @type {ComponentContext} */
    U.r
  ), s = (
    /** @type {Effect} */
    b
  );
  return mr(() => {
    var a, l;
    return yr(() => {
      a = l, l = [], Rr(() => {
        Wt(n(...l), e) || (t(e, ...l), a && Wt(n(...a), e) && t(null, ...a));
      });
    }), () => {
      let o = s;
      for (; o !== i && o.parent !== null && o.parent.f & Xt; )
        o = o.parent;
      const u = () => {
        l && Wt(n(...l), e) && t(null, ...l);
      }, d = o.teardown;
      o.teardown = () => {
        u(), d?.();
      };
    };
  }), e;
}
function Ms(e, t, n, r) {
  var i = (
    /** @type {V} */
    r
  ), s = !0, a = () => (s && (s = !1, i = /** @type {V} */
  r), i);
  e[t];
  var l;
  l = () => {
    var p = (
      /** @type {V} */
      e[t]
    );
    return p === void 0 ? a() : (s = !0, p);
  };
  var o = !1, u = /* @__PURE__ */ jt(() => (o = !1, l())), d = (
    /** @type {Effect} */
    b
  );
  return (
    /** @type {() => V} */
    (function(p, h) {
      if (arguments.length > 0) {
        const _ = h ? f(u) : p;
        return A(u, _), o = !0, i !== void 0 && (i = _), p;
      }
      return Te && o || (d.f & J) !== 0 ? u.v : f(u);
    })
  );
}
const Os = "5";
typeof window < "u" && ((window.__svelte ??= {}).v ??= /* @__PURE__ */ new Set()).add(Os);
const Rs = { sources: [], outputs: [], links: [], groups: [], presets: [], active_preset: null }, yn = "Audio routing";
function zr(e) {
  return e.show_title !== !1 && (e.title ?? yn) !== "";
}
const Ns = (e) => e.show_hint !== !1;
var Ls = /* @__PURE__ */ G('<h1 class="header svelte-9gvyv2"> </h1>'), Ps = /* @__PURE__ */ G('<p class="error svelte-9gvyv2"> </p>'), Ds = /* @__PURE__ */ G('<option disabled="">—</option>'), Is = /* @__PURE__ */ G("<option> </option>"), Fs = /* @__PURE__ */ G('<label class="presetrow svelte-9gvyv2"><span>Preset</span> <select class="svelte-9gvyv2"><!><!></select></label>'), Hs = /* @__PURE__ */ G('<p class="muted svelte-9gvyv2">Loading routing…</p>'), zs = /* @__PURE__ */ G('<p class="muted svelte-9gvyv2">No inputs or outputs yet. Add them in the PipeWire Audio Router add-on.</p>'), js = /* @__PURE__ */ ps('<path></path><path class="hit svelte-9gvyv2" role="button" tabindex="0"></path>', 1), qs = /* @__PURE__ */ G('<span class="sub svelte-9gvyv2">offline</span>'), Bs = /* @__PURE__ */ G('<button><span class="name svelte-9gvyv2"> </span> <!> <span class="dot right-dot svelte-9gvyv2"></span></button>'), Gs = /* @__PURE__ */ G('<span class="sub svelte-9gvyv2"> </span>'), Ys = /* @__PURE__ */ G('<button><span class="dot left-dot svelte-9gvyv2"></span> <span class="name svelte-9gvyv2"> </span> <!></button>'), Us = /* @__PURE__ */ G('<p class="hint svelte-9gvyv2"> </p>'), Vs = /* @__PURE__ */ G('<div><svg class="wires svelte-9gvyv2"></svg> <div class="col left svelte-9gvyv2"></div> <div class="col right svelte-9gvyv2"></div></div> <!>', 1), $s = /* @__PURE__ */ G('<ha-card><!> <div class="body svelte-9gvyv2"><!> <!> <!></div></ha-card>', 2);
const Ws = {
  hash: "svelte-9gvyv2",
  code: `
  /* Everything is expressed in Home Assistant's own theme variables, so the card
     follows the dashboard's theme (including dark mode) with no logic of ours. */.header.svelte-9gvyv2 {font-family:var(--ha-card-header-font-family, inherit);font-size:var(--ha-card-header-font-size, 24px);font-weight:normal;color:var(--ha-card-header-color, var(--primary-text-color));padding:12px 16px 4px;margin:0;letter-spacing:-0.012em;line-height:1.2;}.body.svelte-9gvyv2 {padding:8px 12px 12px;}.muted.svelte-9gvyv2,
  .hint.svelte-9gvyv2 {color:var(--secondary-text-color);font-size:12px;margin:8px 4px 0;}.error.svelte-9gvyv2 {color:var(--error-color, #db4437);font-size:13px;margin:0 4px 8px;}
  /* One row above the graph, deliberately quiet: it changes everything below it,
     so it reads as a mode, not as an action. */.presetrow.svelte-9gvyv2 {display:flex;align-items:center;gap:8px;margin:0 4px 10px;font-size:13px;color:var(--secondary-text-color);}.presetrow.svelte-9gvyv2 select:where(.svelte-9gvyv2) {flex:1 1 auto;min-width:0;font:inherit;color:var(--primary-text-color);background:var(--card-background-color, transparent);border:1px solid var(--divider-color, rgba(127, 127, 127, 0.35));border-radius:8px;padding:5px 8px;}.canvas.svelte-9gvyv2 {position:relative;width:100%;}.canvas.busy.svelte-9gvyv2 {
    /* A call is in flight; the daemon's push is what ends it. Non-interactive
       rather than spinner-ed, so the picture never jumps. */pointer-events:none;opacity:0.65;}.wires.svelte-9gvyv2 {position:absolute;inset:0;overflow:visible;}.wire.svelte-9gvyv2 {fill:none;stroke:var(--primary-color, #03a9f4);stroke-width:2.5;stroke-linecap:round;pointer-events:none;}.wire.partial.svelte-9gvyv2 {stroke-dasharray:6 5;}.wire.off.svelte-9gvyv2 {stroke:var(--disabled-text-color, #bdbdbd);}.wire.live.svelte-9gvyv2 {stroke-width:4;}.hit.svelte-9gvyv2 {fill:none;stroke:transparent;stroke-width:16;cursor:pointer;}.hit.svelte-9gvyv2:focus-visible {outline:none;stroke:var(--primary-color, #03a9f4);stroke-opacity:0.3;}.col.svelte-9gvyv2 {position:absolute;top:0;display:flex;flex-direction:column;gap:8px; /* GAP */padding-top:6px; /* TOP */}.left.svelte-9gvyv2 {left:0;}.right.svelte-9gvyv2 {right:0;}.node.svelte-9gvyv2 {position:relative;box-sizing:border-box;display:flex;flex-direction:column;justify-content:center;gap:1px;width:100%;padding:4px 10px;border:1px solid var(--divider-color, #e0e0e0);border-radius:10px;background:var(--card-background-color, #fff);color:var(--primary-text-color);font:inherit;text-align:left;cursor:pointer;}.node.target.svelte-9gvyv2 {text-align:right;align-items:flex-end;}.node.svelte-9gvyv2:hover {border-color:var(--primary-color, #03a9f4);}.node.held.svelte-9gvyv2 {border-color:var(--primary-color, #03a9f4);box-shadow:0 0 0 1px var(--primary-color, #03a9f4);}.node.absent.svelte-9gvyv2 {color:var(--disabled-text-color, #bdbdbd);}.name.svelte-9gvyv2 {font-size:14px;line-height:1.2;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:100%;}.sub.svelte-9gvyv2 {font-size:11px;color:var(--secondary-text-color);line-height:1.1;}
  /* The wire's anchor, drawn on the edge the wires leave from so a route visibly
     starts at the row rather than floating beside it. */.dot.svelte-9gvyv2 {position:absolute;top:50%;width:8px;height:8px;margin-top:-4px;border-radius:50%;background:var(--divider-color, #e0e0e0);}.right-dot.svelte-9gvyv2 {right:-4px;}.left-dot.svelte-9gvyv2 {left:-4px;}.node.held.svelte-9gvyv2 .dot:where(.svelte-9gvyv2),
  .node.svelte-9gvyv2:hover .dot:where(.svelte-9gvyv2) {background:var(--primary-color, #03a9f4);}`
};
function Ks(e, t) {
  fn(t, !0), Ir(e, Ws);
  let n = Ms(t, "model");
  const r = 6, i = 44, s = 8, a = 84, l = 220, o = 0.36, u = 56;
  let d = /* @__PURE__ */ D(void 0), p = /* @__PURE__ */ D(
    0
    // measured canvas width
  );
  const h = /* @__PURE__ */ O(() => n().snapshot), _ = /* @__PURE__ */ O(() => f(h).links), m = /* @__PURE__ */ O(() => f(h).sources), w = /* @__PURE__ */ O(() => f(h).presets), v = /* @__PURE__ */ O(() => f(w).find((c) => c.id === f(h).active_preset)?.name ?? null), y = /* @__PURE__ */ O(() => new Map(f(h).outputs.map((c) => [c.node_name, c]))), C = /* @__PURE__ */ O(() => new Map(f(m).map((c) => [c.node_name, c]))), x = /* @__PURE__ */ O(() => {
    const c = new Set(f(h).groups.flatMap((g) => g.members));
    return [
      ...f(h).groups.map((g) => ({
        kind: "group",
        key: `g:${g.id}`,
        id: g.id,
        name: g.name,
        members: g.members
      })),
      ...f(h).outputs.filter((g) => !c.has(g.node_name)).map((g) => ({
        kind: "solo",
        key: `o:${g.node_name}`,
        name: g.display_name,
        members: [g.node_name],
        node: g
      }))
    ];
  }), j = (c) => c.members.some((g) => f(y).get(g)?.present ?? !1), X = (c, g) => c.members.filter((k) => f(_).some((V) => V.source === g && V.output === k)), ge = (c) => [
    ...new Set(f(_).filter((g) => c.members.includes(g.output)).map((g) => g.source))
  ], N = /* @__PURE__ */ O(() => Math.max(a, Math.min(l, Math.round(f(p) * o), Math.floor((f(p) - u) / 2)))), me = /* @__PURE__ */ O(() => Math.max(f(N), f(p) - f(N))), ye = (c) => r + c * (i + s) + i / 2, L = (c) => c === 0 ? 0 : r * 2 + c * i + (c - 1) * s, wt = /* @__PURE__ */ O(() => Math.max(i + r * 2, L(f(m).length), L(f(x).length))), qt = /* @__PURE__ */ O(() => new Map(f(m).map((c, g) => [c.node_name, ye(g)]))), jr = /* @__PURE__ */ O(() => new Map(f(x).map((c, g) => [c.key, ye(g)])));
  function qr(c, g, k, V) {
    const fe = Math.max(28, (k - c) * 0.45);
    return `M${c},${g} C${c + fe},${g} ${k - fe},${V} ${k},${V}`;
  }
  const wn = /* @__PURE__ */ O(() => {
    if (f(p) === 0) return [];
    const c = [];
    for (const g of f(x))
      for (const k of ge(g)) {
        const V = f(qt).get(k), fe = f(jr).get(g.key);
        if (V === void 0 || fe === void 0) continue;
        const ue = X(g, k);
        c.push({
          source: k,
          target: g,
          partial: ue.length !== g.members.length,
          off: !(f(C).get(k)?.present ?? !1) || !ue.some((Se) => f(y).get(Se)?.present),
          path: qr(f(N), V, f(me), fe)
        });
      }
    return c;
  });
  let M = /* @__PURE__ */ D(null);
  const bt = /* @__PURE__ */ O(() => {
    const c = f(M);
    return c?.kind === "target" ? f(x).find((g) => g.key === c.key) : void 0;
  });
  Pt(() => {
    const c = f(M);
    c?.kind === "source" && !f(C).has(c.name) && A(M, null), c?.kind === "target" && !f(x).some((g) => g.key === c.key) && A(M, null);
  });
  async function Br(c) {
    if (f(M)?.kind === "source") {
      A(M, f(M).name === c ? null : { kind: "source", name: c }, !0);
      return;
    }
    if (f(bt)) {
      const g = f(bt);
      A(M, null), await bn(c, g);
      return;
    }
    A(M, { kind: "source", name: c }, !0);
  }
  async function Gr(c) {
    if (f(M)?.kind === "target") {
      A(M, f(M).key === c.key ? null : { kind: "target", key: c.key }, !0);
      return;
    }
    if (f(M)?.kind === "source") {
      const g = f(M).name;
      A(M, null), await bn(g, c);
      return;
    }
    A(M, { kind: "target", key: c.key }, !0);
  }
  async function bn(c, g) {
    if (g.members.length === 0) {
      n().error = `“${g.name}” has no speakers yet — add one in the add-on first.`;
      return;
    }
    const k = X(g, c);
    if (g.kind === "group") {
      k.length === g.members.length && g.members.length > 0 ? await n().unrouteGroup(g.id) : await n().routeGroup(g.id, c);
      return;
    }
    k.length ? await n().unlink(c, g.members[0]) : await n().link(c, g.members[0]);
  }
  async function En(c) {
    if (c.target.kind === "group") {
      for (const g of X(c.target, c.source))
        await n().unlink(c.source, g);
      return;
    }
    await n().unlink(c.source, c.target.members[0]);
  }
  const Yr = (c) => `Remove route ${f(C).get(c.source)?.display_name ?? c.source} → ${c.target.name}`;
  function kn(c) {
    if (c.kind === "solo") return j(c) ? "" : "offline";
    if (c.members.length === 0) return "no speakers";
    const g = [
      `${c.members.length} speaker${c.members.length === 1 ? "" : "s"}`
    ];
    return !j(c) && c.members.length ? g.push("offline") : ge(c).length > 1 && g.push("mixed"), g.join(" · ");
  }
  const Ur = /* @__PURE__ */ O(() => f(M)?.kind === "source" ? `Tap where “${f(C).get(f(M).name)?.display_name ?? f(M).name}” should play — or tap it again to cancel.` : f(bt) ? `Tap the input “${f(bt).name}” should play — or tap it again to cancel.` : "Tap an input, then where it should play. Tap a wire to remove a route."), Bt = (c) => f(M)?.kind === "source" && f(M).name === c, Gt = (c) => f(M)?.kind === "target" && f(M).key === c, Vr = (c) => Bt(c.source) || Gt(c.target.key);
  Pt(() => {
    const c = f(d);
    if (!c) return;
    A(p, c.clientWidth, !0);
    const g = new ResizeObserver(([k]) => {
      A(p, Math.round(k.contentRect.width), !0);
    });
    return g.observe(c), () => g.disconnect();
  });
  var xn = $s(), Tn = $(xn);
  {
    var $r = (c) => {
      var g = Ls(), k = $(g);
      de(() => qe(k, n().config.title || yn)), z(c, g);
    }, Wr = /* @__PURE__ */ O(() => zr(n().config));
    ke(Tn, (c) => {
      f(Wr) && c($r);
    });
  }
  var Kr = ee(Tn, 2), An = $(Kr);
  {
    var Xr = (c) => {
      var g = Ps(), k = $(g);
      de(() => qe(k, n().error)), z(c, g);
    };
    ke(An, (c) => {
      n().error && c(Xr);
    });
  }
  var Sn = ee(An, 2);
  {
    var Zr = (c) => {
      var g = Fs(), k = ee($(g), 2), V = $(k);
      {
        var fe = (Q) => {
          var ce = Ds();
          ce.value = ce.__value = "", z(Q, ce);
        };
        ke(V, (Q) => {
          f(v) === null && Q(fe);
        });
      }
      var ue = ee(V);
      Tt(ue, 17, () => f(w), (Q) => Q.id, (Q, ce) => {
        var ze = Is(), Yt = $(ze), Et = {};
        de(() => {
          qe(Yt, f(ce).name), Et !== (Et = f(ce).name) && (ze.value = (ze.__value = f(ce).name) ?? "");
        }), z(Q, ze);
      });
      var Se;
      xs(k), de(() => {
        k.disabled = n().busy, Se !== (Se = f(v) ?? "") && (k.value = (k.__value = f(v) ?? "") ?? "", Fr(k, f(v) ?? ""));
      }), ft("change", k, (Q) => void n().setPreset(Q.currentTarget.value)), z(c, g);
    };
    ke(Sn, (c) => {
      f(w).length > 1 && c(Zr);
    });
  }
  var Jr = ee(Sn, 2);
  {
    var Qr = (c) => {
      var g = Hs();
      z(c, g);
    }, ei = (c) => {
      var g = zs();
      z(c, g);
    }, ti = (c) => {
      var g = Vs(), k = nn(g);
      let V, fe;
      var ue = $(k);
      Tt(ue, 21, () => f(wn), (F) => F.source + " " + F.target.key, (F, S) => {
        var H = js(), we = nn(H);
        let Ce;
        var Me = ee(we);
        de(
          (be, lt) => {
            Ce = At(we, 0, "wire svelte-9gvyv2", null, Ce, be), Re(we, "d", f(S).path), Re(Me, "d", f(S).path), Re(Me, "aria-label", lt);
          },
          [
            () => ({
              off: f(S).off,
              partial: f(S).partial,
              live: Vr(f(S))
            }),
            () => Yr(f(S))
          ]
        ), ft("click", Me, () => En(f(S))), ft("keydown", Me, (be) => {
          (be.key === "Enter" || be.key === " ") && (be.preventDefault(), En(f(S)));
        }), z(F, H);
      });
      var Se = ee(ue, 2);
      let Q;
      Tt(Se, 21, () => f(m), (F) => F.node_name, (F, S) => {
        var H = Bs();
        let we;
        ct(H, "", {}, { height: "44px" });
        var Ce = $(H), Me = $(Ce), be = ee(Ce, 2);
        {
          var lt = (je) => {
            var Ee = qs();
            z(je, Ee);
          };
          ke(be, (je) => {
            f(S).present || je(lt);
          });
        }
        de(
          (je, Ee) => {
            we = At(H, 1, "node svelte-9gvyv2", null, we, je), Re(H, "aria-pressed", Ee), qe(Me, f(S).display_name);
          },
          [
            () => ({
              absent: !f(S).present,
              held: Bt(f(S).node_name)
            }),
            () => Bt(f(S).node_name)
          ]
        ), ft("click", H, () => Br(f(S).node_name)), z(F, H);
      });
      var ce = ee(Se, 2);
      let ze;
      Tt(ce, 21, () => f(x), (F) => F.key, (F, S) => {
        var H = Ys();
        let we;
        ct(H, "", {}, { height: "44px" });
        var Ce = ee($(H), 2), Me = $(Ce), be = ee(Ce, 2);
        {
          var lt = (Ee) => {
            var kt = Gs(), ri = $(kt);
            de((ii) => qe(ri, ii), [() => kn(f(S))]), z(Ee, kt);
          }, je = /* @__PURE__ */ O(() => kn(f(S)));
          ke(be, (Ee) => {
            f(je) && Ee(lt);
          });
        }
        de(
          (Ee, kt) => {
            we = At(H, 1, "node target svelte-9gvyv2", null, we, Ee), Re(H, "aria-pressed", kt), qe(Me, f(S).name);
          },
          [
            () => ({ absent: !j(f(S)), held: Gt(f(S).key) }),
            () => Gt(f(S).key)
          ]
        ), ft("click", H, () => Gr(f(S))), z(F, H);
      }), Hr(k, (F) => A(d, F), () => f(d));
      var Yt = ee(k, 2);
      {
        var Et = (F) => {
          var S = Us(), H = $(S);
          de(() => qe(H, f(Ur))), z(F, S);
        }, ni = /* @__PURE__ */ O(() => Ns(n().config) || f(M));
        ke(Yt, (F) => {
          f(ni) && F(Et);
        });
      }
      de(() => {
        V = At(k, 1, "canvas svelte-9gvyv2", null, V, { busy: n().busy }), fe = ct(k, "", fe, { height: `${f(wt) ?? ""}px` }), Re(ue, "width", f(p)), Re(ue, "height", f(wt)), Re(ue, "aria-hidden", f(wn).length === 0), Q = ct(Se, "", Q, { width: `${f(N) ?? ""}px` }), ze = ct(ce, "", ze, { width: `${f(N) ?? ""}px` });
      }), z(c, g);
    };
    ke(Jr, (c) => {
      n().loaded ? f(m).length === 0 && f(x).length === 0 ? c(ei, 1) : c(ti, -1) : c(Qr);
    });
  }
  z(e, xn), un();
}
cs(["change", "click", "keydown"]);
var Xs = /* @__PURE__ */ G(`<p class="fallback svelte-13lj9sz">Home Assistant's form components aren't available here — use the code (YAML) editor for this card.</p>`), Zs = /* @__PURE__ */ G("<ha-form></ha-form>", 2);
const Js = {
  hash: "svelte-13lj9sz",
  code: ".fallback.svelte-13lj9sz {color:var(--secondary-text-color);font-size:14px;margin:8px 0;}"
};
function Qs(e, t) {
  fn(t, !0), Ir(e, Js);
  const n = "pipewire_audio_router", r = /* @__PURE__ */ O(() => ({
    show_title: t.model.config.show_title !== !1,
    title: t.model.config.title ?? "",
    show_hint: t.model.config.show_hint !== !1,
    entry_id: t.model.config.entry_id ?? ""
  })), i = /* @__PURE__ */ O(() => [
    { name: "show_title", selector: { boolean: {} } },
    // Only worth asking for a heading when there will be one.
    ...f(r).show_title ? [{ name: "title", selector: { text: {} } }] : [],
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
  }, a = {
    title: `Leave empty for “${yn}”.`,
    show_hint: "The line under the graph explaining how to route. It reappears by itself while an input is held.",
    entry_id: "Only needed if you have more than one PipeWire Audio Router configured."
  }, l = (v) => s[v.name] ?? v.name, o = (v) => a[v.name] ?? "";
  function u(v) {
    const y = {
      type: t.model.config.type ?? "custom:pipewire-router-card"
    };
    v.show_title === !1 && (y.show_title = !1), typeof v.title == "string" && v.title !== "" && (y.title = v.title), v.show_hint === !1 && (y.show_hint = !1), typeof v.entry_id == "string" && v.entry_id !== "" && (y.entry_id = v.entry_id), t.model.onChange(y);
  }
  let d = /* @__PURE__ */ D(void 0);
  const p = /* @__PURE__ */ O(() => !!t.model.hass && !customElements.get("ha-form"));
  Pt(() => {
    const v = f(d);
    v && (v.hass = t.model.hass, v.schema = f(i), v.data = f(r), v.computeLabel = l, v.computeHelper = o);
  }), Pt(() => {
    const v = f(d);
    if (!v) return;
    const y = (C) => {
      const x = C.detail?.value;
      x && u(x);
    };
    return v.addEventListener("value-changed", y), () => v.removeEventListener("value-changed", y);
  });
  var h = _s(), _ = nn(h);
  {
    var m = (v) => {
      var y = Xs();
      z(v, y);
    }, w = (v) => {
      var y = Zs();
      Hr(y, (C) => A(d, C), () => f(d)), z(v, y);
    };
    ke(_, (v) => {
      f(p) ? v(m) : t.model.hass && v(w, 1);
    });
  }
  z(e, h), un();
}
const Bn = "pipewire_audio_router";
class eo {
  #e = /* @__PURE__ */ D(Le(Rs));
  get snapshot() {
    return f(this.#e);
  }
  set snapshot(t) {
    A(this.#e, t, !0);
  }
  #t = /* @__PURE__ */ D(Le({}));
  get config() {
    return f(this.#t);
  }
  set config(t) {
    A(this.#t, t, !0);
  }
  #n = /* @__PURE__ */ D(!1);
  get loaded() {
    return f(this.#n);
  }
  set loaded(t) {
    A(this.#n, t, !0);
  }
  #a = /* @__PURE__ */ D(null);
  get error() {
    return f(this.#a);
  }
  set error(t) {
    A(this.#a, t, !0);
  }
  #s = /* @__PURE__ */ D(!1);
  get busy() {
    return f(this.#s);
  }
  set busy(t) {
    A(this.#s, t, !0);
  }
  #o = null;
  #r = null;
  #l = null;
  #i = 0;
  setConfig(t) {
    const n = this.config.entry_id;
    this.config = t, t.entry_id !== n && this.#o && this.#h();
  }
  /** Called by the element on every `hass` update — which in Home Assistant is
   *  every state change in the house, so this must be cheap and must not
   *  resubscribe. `connection` is stable for the life of the frontend session,
   *  so it is the thing worth comparing. */
  setHass(t) {
    this.#o = t, t.connection !== this.#r && (this.#r = t.connection, this.#h());
  }
  async #h() {
    const t = this.#o;
    if (!t) return;
    const n = ++this.#i;
    await this.#u();
    try {
      const r = await t.connection.subscribeMessage(
        (i) => {
          n === this.#i && (this.snapshot = i, this.loaded = !0, this.error = null);
        },
        { type: `${Bn}/subscribe`, ...this.#c() }
      );
      if (n !== this.#i) {
        r();
        return;
      }
      this.#l = r;
    } catch (r) {
      if (n !== this.#i) return;
      this.error = Gn(r), this.loaded = !0;
    }
  }
  async #u() {
    const t = this.#l;
    if (this.#l = null, !!t)
      try {
        await t();
      } catch {
      }
  }
  /** Detached from the DOM: drop the subscription, and forget which connection we
   *  were on so re-attaching (the same element, on the same session, when its view
   *  comes back) subscribes again instead of sitting silent. */
  disconnect() {
    this.#i++, this.#r = null, this.#u();
  }
  #c() {
    return this.config.entry_id ? { entry_id: this.config.entry_id } : {};
  }
  async #f(t, n) {
    const r = this.#o;
    if (!(!r || this.busy)) {
      this.busy = !0;
      try {
        await r.callWS({ type: `${Bn}/${t}`, ...this.#c(), ...n }), this.error = null;
      } catch (i) {
        this.error = Gn(i);
      } finally {
        this.busy = !1;
      }
    }
  }
  /** Route one source into one lone output (additive: an output can mix several). */
  link(t, n) {
    return this.#f("link", { source: t, output: n });
  }
  unlink(t, n) {
    return this.#f("unlink", { source: t, output: n });
  }
  /** Put a whole group on one source — exclusive, so this also drops whatever
   *  else its members were playing. Same call as the group's Source dropdown. */
  routeGroup(t, n) {
    return this.#f("route_group", { group_id: t, source: n });
  }
  unrouteGroup(t) {
    return this.#f("unroute_group", { group_id: t });
  }
  /** Put a whole preset in force: every group's membership *and* what it plays,
   *  in one daemon operation. Accepts an id or a name. */
  setPreset(t) {
    return this.#f("set_preset", { preset: t });
  }
}
function Gn(e) {
  if (typeof e == "object" && e !== null && "message" in e) {
    const t = e.message;
    if (typeof t == "string" && t) return t;
  }
  return e instanceof Error && e.message ? e.message : "Home Assistant rejected the request";
}
class to {
  #e = /* @__PURE__ */ D(Le({}));
  get config() {
    return f(this.#e);
  }
  set config(t) {
    A(this.#e, t, !0);
  }
  #t = /* @__PURE__ */ D(null);
  get hass() {
    return f(this.#t);
  }
  set hass(t) {
    A(this.#t, t, !0);
  }
  onChange = () => {
  };
}
const st = "pipewire-router-card", an = `${st}-editor`;
class no extends HTMLElement {
  #e = new eo();
  #t = null;
  connectedCallback() {
    this.#t || (this.#t = Pr(Ks, { target: this, props: { model: this.#e } }));
  }
  disconnectedCallback() {
    this.#t && (Dr(this.#t), this.#t = null), this.#e.disconnect();
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
    return (zr(this.#e.config) ? 1 : 0) + Math.max(1, this.#a());
  }
  #a() {
    const { sources: t, outputs: n, groups: r } = this.#e.snapshot, i = new Set(r.flatMap((a) => a.members)), s = r.length + n.filter((a) => !i.has(a.node_name)).length;
    return Math.max(t.length, s);
  }
  /** What the card picker inserts. No options needed for the common case of one
   *  configured router. */
  static getStubConfig() {
    return { type: `custom:${st}` };
  }
  /** The visual editor behind the card's "Edit" pane. Without this, Lovelace only
   *  offers the YAML editor for a custom card. */
  static async getConfigElement() {
    return await ro(), document.createElement(an);
  }
}
async function ro() {
  if (!customElements.get("ha-form"))
    try {
      const e = window.loadCardHelpers;
      await (await (await e?.())?.createCardElement?.({ type: "entities", entities: [] }))?.constructor?.getConfigElement?.();
    } catch {
    }
}
class io extends HTMLElement {
  #e = new to();
  #t = null;
  constructor() {
    super(), this.#e.onChange = (t) => {
      this.dispatchEvent(
        new CustomEvent("config-changed", { detail: { config: t }, bubbles: !0, composed: !0 })
      );
    };
  }
  connectedCallback() {
    this.#t || (this.#t = Pr(Qs, { target: this, props: { model: this.#e } }));
  }
  disconnectedCallback() {
    this.#t && (Dr(this.#t), this.#t = null);
  }
  setConfig(t) {
    this.#e.config = { ...t };
  }
  set hass(t) {
    this.#e.hass = t;
  }
}
customElements.get(st) || customElements.define(st, no);
customElements.get(an) || customElements.define(an, io);
const Yn = window.customCards ??= [];
Yn.some((e) => e.type === st) || Yn.push({
  type: st,
  name: "PipeWire Audio Routing",
  description: "All audio routing at a glance — tap an input, then where it should play.",
  // The picker renders a live card: it is the real graph, and reading it is
  // harmless (nothing is routed until something is tapped).
  preview: !0,
  documentationURL: "https://github.com/davidgraeff/homeassistant-audio-routing"
});
