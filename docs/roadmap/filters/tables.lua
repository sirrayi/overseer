-- Tables → #roadmap-table(cols:..., header:..., cells:...) raw typst.
-- Column width model: every column gets at least the width of its
-- longest unbreakable segment (words split on / _ . : - too, since
-- inline code may break after those), plus a share of what's left in
-- proportion to its longest cell — so a column never overflows and a
-- narrow id column never eats a prose column's share.

local function blocks_text(blocks)
  return pandoc.utils.stringify(pandoc.Pandoc(blocks))
end

local function cell_typst(cell)
  local s = pandoc.write(pandoc.Pandoc(cell.contents), "typst")
  return "[" .. s:gsub("^%s+", ""):gsub("%s+$", "") .. "]"
end

local function longest_segment(text)
  local best = 0
  for word in text:gmatch("%S+") do
    for seg in word:gmatch("[^/._:%-]+") do
      if #seg > best then
        best = #seg
      end
    end
  end
  return best
end

function Table(el)
  local ncols = #el.colspecs
  local wants, needs = {}, {}
  for i = 1, ncols do
    wants[i], needs[i] = 0, 0
  end

  local function measure(row)
    for i, cell in ipairs(row.cells) do
      if i <= ncols then
        local text = blocks_text(cell.contents)
        if #text > wants[i] then
          wants[i] = #text
        end
        local seg = longest_segment(text)
        if seg > needs[i] then
          needs[i] = seg
        end
      end
    end
  end

  for _, row in ipairs(el.head.rows) do
    measure(row)
  end
  for _, body in ipairs(el.bodies) do
    for _, row in ipairs(body.body) do
      measure(row)
    end
  end

  -- ~0.0115 of the text width per 8.5 pt Charter char, + inset buffer.
  local need_sum, want_sum = 0, 0
  for i = 1, ncols do
    needs[i] = needs[i] * 0.0115 + 0.028
    wants[i] = math.max(wants[i], 8)
    need_sum = need_sum + needs[i]
    want_sum = want_sum + wants[i]
  end
  if need_sum > 0.9 then
    for i = 1, ncols do
      needs[i] = needs[i] * 0.9 / need_sum
    end
    need_sum = 0.9
  end
  local share = 1 - need_sum

  local col_parts = {}
  for i = 1, ncols do
    col_parts[i] = string.format("%.4f", needs[i] + wants[i] / want_sum * share)
  end

  local hdr_parts = {}
  for _, row in ipairs(el.head.rows) do
    for i, cell in ipairs(row.cells) do
      if i <= ncols then
        hdr_parts[#hdr_parts + 1] = cell_typst(cell)
      end
    end
  end

  local cell_parts = {}
  for _, body in ipairs(el.bodies) do
    for _, row in ipairs(body.body) do
      for i, cell in ipairs(row.cells) do
        if i <= ncols then
          cell_parts[#cell_parts + 1] = cell_typst(cell)
        end
      end
    end
  end

  local out = "#roadmap-table(\n"
    .. "  cols: (" .. table.concat(col_parts, ", ") .. ",),\n"
    .. "  header: (" .. table.concat(hdr_parts, ", ") .. ",),\n"
    .. "  cells: (\n    " .. table.concat(cell_parts, ",\n    ") .. ",\n  ),\n"

  local cap = el.caption and el.caption.long and blocks_text(el.caption.long) or ""
  if #cap > 0 then
    local s = pandoc.write(pandoc.Pandoc(el.caption.long), "typst")
    out = out .. "  caption: [" .. s:gsub("^%s+", ""):gsub("%s+$", "") .. "],\n"
  end
  out = out .. ")"

  return pandoc.RawBlock("typst", out)
end
