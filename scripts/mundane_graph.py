#!/usr/bin/env python3
"""Generate a deliberately mundane example graph as JSON Lines.

    python3 scripts/mundane_graph.py > mundane.jsonl
    glider mundane.gldb import mundane.jsonl
    glider mundane.gldb browser

250,000 nodes of small-town commerce — people, the cities they live in, the
companies they work for, the products those companies sell, the orders people
place and the reviews they leave — with a mix of property types (text, int,
float, bool, list, and the odd null) so there is something to edit in the
explorer. Deterministic: the same seed gives the same graph.

Nodes                              Relationships
  150,000 Person                     ~450k KNOWS       Person -> Person
   60,000 Order                       150k LIVES_IN    Person -> City
   20,000 Product                     135k WORKS_AT    Person -> Company
   10,000 Review                       60k PLACED      Person -> Order
    8,000 Company                     ~180k CONTAINS   Order  -> Product
    1,500 City                         10k WROTE       Person -> Review
      500 Category                     10k ABOUT       Review -> Product
                                       20k IN_CATEGORY Product -> Category
                                       20k SELLS       Company -> Product
                                        8k BASED_IN    Company -> City
"""

import json
import random
import sys

SEED = 20260921
N_PERSON, N_ORDER, N_PRODUCT, N_REVIEW, N_COMPANY, N_CITY, N_CATEGORY = (
    150_000, 60_000, 20_000, 10_000, 8_000, 1_500, 500,
)
assert N_PERSON + N_ORDER + N_PRODUCT + N_REVIEW + N_COMPANY + N_CITY + N_CATEGORY == 250_000

FIRST = """Aaron Abigail Adam Adrian Aisha Alan Alice Amelia Amir Andrew Anna Anthony Arjun Arthur Ava
Beatrice Ben Bethany Brian Callum Carol Charlie Charlotte Chloe Christopher Claire Connor Daniel David
Deborah Dylan Edward Eleanor Elizabeth Ella Emily Emma Ethan Eva Fatima Fiona Florence Frank Freya
Gavin George Grace Hannah Harry Hassan Heather Helen Henry Holly Hugo Ian Imogen Isaac Isabella Jack
Jacob James Jamie Jasmine Jennifer Jessica John Jonathan Joseph Joshua Julia Karen Katie Kevin Laura
Leah Leo Liam Lily Louise Lucas Lucy Luke Margaret Maria Mark Martin Mary Matthew Maya Megan Mia
Michael Mohammed Molly Nathan Neil Nicola Noah Oliver Olivia Omar Oscar Patrick Paul Peter Philip
Poppy Priya Rachel Rebecca Richard Robert Rosie Ruby Ryan Samuel Sarah Sean Sofia Sophie Stephen
Susan Thomas Tom Victoria William Yasmin Zachary Zara""".split()

LAST = """Adams Ahmed Ali Allen Anderson Bailey Baker Begum Bell Bennett Brown Butler Campbell Carter
Chapman Clark Clarke Collins Cook Cooper Cox Davies Davis Dixon Edwards Ellis Evans Fisher Foster Fox
Gibson Graham Gray Green Griffiths Hall Hamilton Harris Harrison Hill Holmes Hughes Hunt Hussain Jackson
James Jenkins Johnson Jones Kelly Kennedy Khan King Knight Lee Lewis Lloyd Marshall Martin Mason Matthews
Miller Mitchell Moore Morgan Morris Murphy Murray Owen Palmer Parker Patel Pearson Phillips Powell Price
Reid Reynolds Richards Richardson Roberts Robinson Rogers Rose Russell Saunders Scott Shaw Simpson Singh
Smith Stevens Stewart Taylor Thomas Thompson Turner Walker Walsh Ward Watson Webb White Wilkinson Williams
Wilson Wood Wright Young""".split()

REAL_CITIES = """London Manchester Birmingham Leeds Glasgow Liverpool Bristol Sheffield Edinburgh Cardiff
Belfast Newcastle Nottingham Leicester Coventry Bradford Brighton Southampton Portsmouth Plymouth Derby
Norwich Exeter York Bath Oxford Cambridge Aberdeen Dundee Swansea Reading Luton Preston Blackpool
Sunderland Wolverhampton Stoke Ipswich Chester Lincoln Durham Carlisle Lancaster Worcester Gloucester
Hereford Salisbury Winchester Canterbury Chichester Truro Dublin Cork Galway Limerick Paris Lyon
Marseille Toulouse Nice Nantes Berlin Hamburg Munich Cologne Frankfurt Stuttgart Leipzig Dresden
Amsterdam Rotterdam Utrecht Brussels Antwerp Ghent Copenhagen Aarhus Oslo Bergen Stockholm Gothenburg
Malmo Helsinki Tampere Madrid Barcelona Valencia Seville Bilbao Lisbon Porto Rome Milan Naples Turin
Florence Bologna Vienna Graz Zurich Geneva Basel Prague Brno Warsaw Krakow Gdansk Budapest Athens
Toronto Vancouver Montreal Calgary Ottawa Boston Chicago Seattle Portland Denver Austin Atlanta
Sydney Melbourne Brisbane Perth Adelaide Auckland Wellington Tokyo Osaka Kyoto Singapore""".split()
SYL_A = "Ash Bar Bel Brad Bur Car Chad Cran Dun Eas Fair Glen Har Hol Kings Lang Mar Mel Nor Oak Pen Red Rush Sal Shel Stan Sut Thorn Wal Wes Win Wood".split()
SYL_B = "ford ton bury wick field ham mouth stead port bridge leigh worth dale by minster caster".split()
COUNTRIES = ["UK", "Ireland", "France", "Germany", "Netherlands", "Belgium", "Denmark", "Norway", "Sweden", "Finland", "Spain", "Portugal", "Italy", "Austria", "Switzerland", "Czechia", "Poland", "Hungary", "Greece", "Canada", "USA", "Australia", "New Zealand", "Japan", "Singapore"]

CATEGORY_ROOTS = """Kitchen Garden Bathroom Bedroom Office Garage Stationery Lighting Storage Cleaning
Cookware Tableware Textiles Tools Hardware Paint Plumbing Electrical Pet Toys Sports Camping Cycling
Luggage Footwear Knitwear Outerwear Accessories Jewellery Watches Books Music Games Puzzles Crafts
Baking Coffee Tea Snacks Confectionery""".split()
CATEGORY_MODS = "Essentials Basics Premium Classic Seasonal Everyday Budget Outdoor Indoor Kids Compact Heavy-Duty".split()
ADJ = "Stainless Wooden Ceramic Cotton Linen Woollen Glass Bamboo Recycled Folding Stackable Insulated Non-stick Cordless Rechargeable Compact Large Small Medium Extra-Long Heavy Lightweight Waterproof Matte Glossy Striped Plain Checked".split()
NOUN = """Kettle Toaster Mug Teapot Saucepan Frying-Pan Chopping-Board Colander Whisk Ladle Spatula
Bowl Plate Tumbler Jug Tray Tin Jar Canister Lunchbox Flask Towel Bath-Mat Shower-Curtain Soap-Dish
Toothbrush-Holder Bin Laundry-Basket Peg-Bag Clothes-Horse Iron Hanger Duvet Pillow Sheet Blanket
Throw Cushion Curtain Blind Rug Doormat Lamp Bulb Torch Extension-Lead Adaptor Battery Notebook
Pen Pencil Ruler Stapler Folder Envelope Calendar Diary Planner Trowel Spade Fork Rake Hose Watering-Can
Plant-Pot Seed-Tray Secateurs Gloves Wellies Umbrella Backpack Suitcase Holdall Wallet Belt Scarf Hat
Socks Slippers Trainers Boots Sandals Jumper Cardigan Coat Jacket Fleece Tent Sleeping-Bag Stove
Cool-Box Bike-Pump Puncture-Kit Helmet Lock Bell Board-Game Jigsaw Yarn Needles Cake-Tin Rolling-Pin
Sieve Coffee-Grinder Cafetiere Tea-Strainer Biscuit-Tin""".split()
COMPANY_SUFFIX = ["Ltd", "& Sons", "& Daughters", "Co.", "Supplies", "Trading", "Stores", "Brothers", "Group", "Direct", "Wholesale", "Hardware", "Homewares", "Depot", "Emporium"]
STATUS = ["delivered"] * 14 + ["shipped"] * 3 + ["processing"] * 2 + ["cancelled", "returned"]
PAYMENT = ["card", "card", "card", "paypal", "bank transfer", "gift card"]
COURIER = ["Royal Mail", "DPD", "Evri", "UPS", "DHL", "Yodel"]
STREETS = "High Street|Church Lane|Station Road|Mill Lane|Park Avenue|Victoria Road|The Green|King's Road|Queen Street|Bridge Street|Market Square|Orchard Close|Meadow Way|School Lane|Manor Drive".split("|")
DEPTS = "Sales Support Warehouse Accounts Marketing Engineering Dispatch Buying HR Reception Maintenance Design".split()
JOBS = "Assistant Manager Supervisor Clerk Analyst Coordinator Specialist Technician Administrator Apprentice Lead Associate".split()
HOBBIES = "gardening cycling baking hiking knitting chess running fishing photography reading painting swimming birdwatching pottery football cooking board-games camping crosswords DIY".split()
REVIEW_GOOD = ["Does exactly what it says.", "Arrived quickly, well packaged.", "Good value for the price.", "Sturdier than I expected.", "Second one I've bought — first is still going strong.", "Fits neatly in the cupboard.", "Nice colour, matches the kitchen.", "Would buy again."]
REVIEW_MEH = ["Fine, nothing special.", "Slightly smaller than the picture suggests.", "Works, but the instructions were unclear.", "OK for the money.", "Took a while to arrive.", "Average. Does the job."]
REVIEW_BAD = ["Broke after two weeks.", "Not as described.", "Handle came loose almost immediately.", "Returned it — wrong size.", "Smelled of plastic for days.", "Disappointing quality."]


def main():
    out = sys.stdout
    rng = random.Random(SEED)
    # Edges must reference nodes that appear earlier in the file, and KNOWS
    # points forward, so nodes stream out as they are made and every edge
    # waits until the end.
    edges = []

    def write(obj):
        line = json.dumps(obj, separators=(",", ":")) + "\n"
        if obj["type"] == "edge":
            edges.append(line)
        else:
            out.write(line)

    def date(y0, y1):
        y = rng.randint(y0, y1)
        m = rng.randint(1, 12)
        d = rng.randint(1, 28)
        return f"{y:04d}-{m:02d}-{d:02d}"

    # ---- cities
    cities = []
    for i in range(N_CITY):
        if i < len(REAL_CITIES):
            name = REAL_CITIES[i]
        else:
            name = rng.choice(SYL_A) + rng.choice(SYL_B)
            if name in cities:
                name += " " + rng.choice(["North", "South", "East", "West", "Green", "Cross"])
        cities.append(name)
        write({"type": "node", "key": f"city{i}", "labels": ["City"], "props": {
            "name": name, "country": rng.choice(COUNTRIES),
            "population": int(rng.lognormvariate(10, 1.2)), "coastal": rng.random() < 0.3,
        }})
    # Most people live in the big places: a skewed distribution over cities.
    city_weights = [1.0 / (i + 1) ** 0.6 for i in range(N_CITY)]

    # ---- categories
    cats = []
    for i in range(N_CATEGORY):
        root = CATEGORY_ROOTS[i % len(CATEGORY_ROOTS)]
        name = root if i < len(CATEGORY_ROOTS) else f"{root} {CATEGORY_MODS[(i // len(CATEGORY_ROOTS)) % len(CATEGORY_MODS)]}"
        cats.append(name)
        write({"type": "node", "key": f"cat{i}", "labels": ["Category"], "props": {"name": name, "aisle": rng.randint(1, 40)}})

    # ---- companies
    companies = []
    for i in range(N_COMPANY):
        surname = rng.choice(LAST)
        kind = rng.random()
        if kind < 0.4:
            name = f"{surname} {rng.choice(COMPANY_SUFFIX)}"
        elif kind < 0.7:
            name = f"{surname} & {rng.choice(LAST)}"
        else:
            name = f"{rng.choice(SYL_A)}{rng.choice(SYL_B)} {rng.choice(COMPANY_SUFFIX)}"
        companies.append(name)
        city = rng.choices(range(N_CITY), city_weights)[0]
        write({"type": "node", "key": f"co{i}", "labels": ["Company"], "props": {
            "name": name, "founded": rng.randint(1890, 2024), "employees": int(rng.lognormvariate(3, 1.1)) + 1,
            "vat_registered": rng.random() < 0.8, "website": f"www.{surname.lower()}{rng.randint(1, 99)}.example",
        }})
        write({"type": "edge", "from": f"co{i}", "to": f"city{city}", "label": "BASED_IN", "props": {"since": rng.randint(1990, 2026)}})

    # ---- products
    for i in range(N_PRODUCT):
        name = f"{rng.choice(ADJ)} {rng.choice(NOUN).replace('-', ' ')}"
        price = round(rng.choice([1.99, 2.49, 3.99, 4.99, 6.5, 7.99, 9.99, 12.0, 14.99, 19.99, 24.99, 29.0, 34.99, 49.99, 79.0, 119.0]) * rng.choice([1, 1, 1, 1.1, 0.9]), 2)
        props = {"name": name, "sku": f"SKU-{i:06d}", "price": price, "in_stock": rng.random() < 0.85, "weight_kg": round(rng.uniform(0.05, 12), 2)}
        if rng.random() < 0.3:
            props["colour"] = rng.choice(["black", "white", "grey", "navy", "green", "red", "natural", "cream"])
        if rng.random() < 0.15:
            props["discontinued"] = True
        write({"type": "node", "key": f"prod{i}", "labels": ["Product"], "props": props})
        write({"type": "edge", "from": f"prod{i}", "to": f"cat{rng.randrange(N_CATEGORY)}", "label": "IN_CATEGORY", "props": {}})
        write({"type": "edge", "from": f"co{rng.randrange(N_COMPANY)}", "to": f"prod{i}", "label": "SELLS", "props": {"margin": round(rng.uniform(0.05, 0.6), 2)}})

    # ---- people
    for i in range(N_PERSON):
        first, last = rng.choice(FIRST), rng.choice(LAST)
        born = rng.randint(1940, 2007)
        props = {
            "name": f"{first} {last}",
            "email": f"{first.lower()}.{last.lower()}{i % 977}@example.com",
            "born": born,
            "phone": f"07{rng.randint(100, 999)} {rng.randint(100000, 999999)}",
            "joined": date(2012, 2026),
            "newsletter": rng.random() < 0.4,
        }
        if rng.random() < 0.5:
            props["hobbies"] = rng.sample(HOBBIES, rng.randint(1, 3))
        if rng.random() < 0.7:
            props["address"] = f"{rng.randint(1, 180)} {rng.choice(STREETS)}"
        if rng.random() < 0.05:
            props["notes"] = None
        write({"type": "node", "key": f"p{i}", "labels": ["Person"] + (["Staff"] if i % 40 == 0 else []), "props": props})
        write({"type": "edge", "from": f"p{i}", "to": f"city{rng.choices(range(N_CITY), city_weights)[0]}", "label": "LIVES_IN", "props": {"since": rng.randint(max(born + 16, 1990), 2026)}})
        if rng.random() < 0.9 and 2026 - born >= 18:
            write({"type": "edge", "from": f"p{i}", "to": f"co{rng.randrange(N_COMPANY)}", "label": "WORKS_AT", "props": {
                "role": f"{rng.choice(DEPTS)} {rng.choice(JOBS)}", "since": rng.randint(max(born + 18, 1995), 2026), "part_time": rng.random() < 0.25,
            }})
        # Friends are mostly nearby in id space — people who joined around the same time.
        for _ in range(3):
            j = min(N_PERSON - 1, max(0, i + int(rng.gauss(0, 400)))) if rng.random() < 0.8 else rng.randrange(N_PERSON)
            if j != i:
                write({"type": "edge", "from": f"p{i}", "to": f"p{j}", "label": "KNOWS", "props": {"since": rng.randint(2005, 2026), "how": rng.choice(["school", "work", "neighbours", "family", "club", "online"])}})

    # ---- orders
    for i in range(N_ORDER):
        n_items = rng.choice([1, 1, 2, 2, 3, 3, 4, 5, 6])
        total = 0.0
        items = [rng.randrange(N_PRODUCT) for _ in range(n_items)]
        placed = date(2019, 2026)
        write({"type": "node", "key": f"o{i}", "labels": ["Order"], "props": {
            "ref": f"ORD-{2019 + i % 8}-{i:06d}", "placed": placed, "status": rng.choice(STATUS),
            "payment": rng.choice(PAYMENT), "courier": rng.choice(COURIER), "items": n_items, "gift": rng.random() < 0.08,
        }})
        write({"type": "edge", "from": f"p{rng.randrange(N_PERSON)}", "to": f"o{i}", "label": "PLACED", "props": {"on": placed}})
        for p in items:
            write({"type": "edge", "from": f"o{i}", "to": f"prod{p}", "label": "CONTAINS", "props": {"qty": rng.choice([1, 1, 1, 2, 2, 3, 6])}})

    # ---- reviews
    for i in range(N_REVIEW):
        stars = rng.choices([1, 2, 3, 4, 5], [5, 7, 15, 35, 38])[0]
        text = rng.choice(REVIEW_GOOD if stars >= 4 else REVIEW_MEH if stars == 3 else REVIEW_BAD)
        write({"type": "node", "key": f"r{i}", "labels": ["Review"], "props": {
            "stars": stars, "text": text, "posted": date(2020, 2026), "verified": rng.random() < 0.7, "helpful": rng.randint(0, 40),
        }})
        write({"type": "edge", "from": f"p{rng.randrange(N_PERSON)}", "to": f"r{i}", "label": "WROTE", "props": {}})
        write({"type": "edge", "from": f"r{i}", "to": f"prod{rng.randrange(N_PRODUCT)}", "label": "ABOUT", "props": {}})

    out.writelines(edges)


if __name__ == "__main__":
    main()
