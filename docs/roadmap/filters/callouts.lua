-- Fenced divs → #callout(kind: "...") raw typst blocks.
local kinds = { "decision", "risk", "gate", "note", "principle" }

local function kind_of(el)
  for _, k in ipairs(kinds) do
    if el.classes:includes(k) then
      return k
    end
  end
end

function Div(el)
  local kind = kind_of(el)
  if not kind then
    return nil
  end
  local inner = pandoc.write(pandoc.Pandoc(el.content), "typst")
  return pandoc.RawBlock(
    "typst",
    "#callout(kind: \"" .. kind .. "\")[\n" .. inner .. "\n]"
  )
end
